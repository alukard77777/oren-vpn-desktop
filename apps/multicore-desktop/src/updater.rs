use multicore_i18n::tr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use multicore_release::{
    DownloadError, DownloadProgress, GitHubRelease, ReleaseDecision, Repository, Version,
    receive_exact, verify_archive_file,
};

const RELEASE_BODY_LIMIT: usize = 512 * 1024;
/// `release.txt` holds one version and a newline; anything longer is not that file.
const RELEASE_HINT_LIMIT: usize = 64;
/// Without a usable `release.txt` the releases API is asked at most this often (it allows 60
/// unauthenticated requests an hour per IP, shared by every client behind one NAT).
pub(crate) const API_FALLBACK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
/// When the releases API was last asked, for the fallback cadence.
static LAST_API_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
const UPDATE_USER_AGENT: &str = concat!("multicore-updater/", env!("CARGO_PKG_VERSION"));
/// What an installed update replaces.
fn replacement_detail() -> &'static str {
    if cfg!(target_os = "linux") {
        tr!("Будет заменён файл AppImage")
    } else {
        tr!("Будет заменена установленная версия")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum UpdateAction {
    #[default]
    None,
    Check,
    Install,
}

#[derive(Clone, Debug)]
pub(crate) enum UpdateState {
    Unconfigured,
    Checking,
    Current,
    Available(GitHubRelease),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Managed(GitHubRelease),
    /// The user started installing `release`; `UpdateStage` says how far it got.
    Updating(GitHubRelease, UpdateStage),
    /// Installing `release` failed; the release is kept so the banner stays
    /// visible with the reason and the install can be retried.
    InstallFailed(GitHubRelease, String),
    /// GitHub asked us to slow down (or nothing could be learned yet): no error, the next
    /// scheduled check tries again.
    Deferred,
    Error(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UpdatePresentation {
    pub status: String,
    pub detail: String,
    pub action_label: String,
    pub action: UpdateAction,
    pub action_enabled: bool,
    pub available: bool,
    pub busy: bool,
    /// The home toast (shown while `available`): its title and its one line under it.
    pub toast_title: String,
    pub toast_copy: String,
    /// What «Позже»/«Скрыть» hides the toast for: the same offer, the same run or the same
    /// failure stays hidden, the next one shows again.
    pub toast_key: String,
    /// An update runs: the progress bar is shown instead of an action.
    pub in_progress: bool,
    /// The bar's fill in thousandths; -1 while the size is not known (no fill is drawn).
    pub progress_permille: i32,
    pub failed: bool,
}

/// Where a running update is. Stages only move forward (see `UpdateStage::precedes`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateStage {
    /// `received` of `total` bytes are in; `total` 0: the size is not known yet.
    Downloading(DownloadProgress),
    /// The download is complete; its size and SHA-256 are checked.
    Verifying,
    /// The verified update is handed to the updater or the service (after the tunnel is
    /// stopped, when connected).
    Installing,
    /// The updater took over: the app quits and comes back as the new version.
    Restarting,
}

impl UpdateStage {
    fn rank(self) -> u8 {
        match self {
            Self::Downloading(_) => 0,
            Self::Verifying => 1,
            Self::Installing => 2,
            Self::Restarting => 3,
        }
    }

    /// Whether `next` moves a run at `self` forward: a later stage, or more bytes of the same
    /// download. A late report from a slower thread never moves the bar back.
    pub(crate) fn precedes(self, next: Self) -> bool {
        match (self, next) {
            (Self::Downloading(now), Self::Downloading(then)) => {
                then.received > now.received || (now.total == 0 && then.total > 0)
            }
            _ => next.rank() > self.rank(),
        }
    }

    /// The bar's fill in thousandths, -1 while the download's size is unknown.
    pub(crate) fn permille(self) -> i32 {
        match self {
            Self::Downloading(progress) if progress.total == 0 => -1,
            Self::Downloading(progress) => {
                let permille = u128::from(progress.received.min(progress.total)) * 1000
                    / u128::from(progress.total);
                i32::try_from(permille).unwrap_or(1000)
            }
            Self::Verifying | Self::Installing | Self::Restarting => 1000,
        }
    }

    /// «Скачиваем… 34 % · 18 из 52 МБ», «Проверяем…», «Устанавливаем…», «Перезапуск…».
    pub(crate) fn text(self) -> String {
        match self {
            Self::Downloading(progress) if progress.total > 0 => tr!(
                "Скачиваем… {} % · {} из {} МБ",
                progress.percent(),
                megabytes(progress.received),
                megabytes(progress.total)
            ),
            Self::Downloading(_) => tr!("Скачиваем…").into(),
            Self::Verifying => tr!("Проверяем…").into(),
            Self::Installing => tr!("Устанавливаем…").into(),
            Self::Restarting => tr!("Перезапуск…").into(),
        }
    }
}

/// Whole MiB, rounded to the nearest so the received and total figures agree at the end.
pub(crate) fn megabytes(bytes: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    bytes.saturating_add(MIB / 2) / MIB
}

impl UpdateState {
    pub(crate) fn initial() -> Self {
        match configured_repository() {
            Ok(Some(_)) => Self::Checking,
            Ok(None) => Self::Unconfigured,
            Err(error) => Self::Error(error),
        }
    }

    pub(crate) fn presentation(&self, runtime_idle: bool) -> UpdatePresentation {
        let configured = configured_repository().ok().flatten().is_some();
        self.presentation_with_configuration(runtime_idle, configured)
    }

    pub(crate) fn presentation_with_configuration(
        &self,
        runtime_idle: bool,
        configured: bool,
    ) -> UpdatePresentation {
        let mut shown = self.row_presentation(runtime_idle, configured);
        match self {
            Self::Available(release) | Self::Managed(release) => {
                shown.toast_title = tr!("Доступно обновление").into();
                shown.toast_copy = shown.status.clone();
                shown.toast_key = format!("offer:{}", release.version());
            }
            Self::Updating(release, stage) => {
                shown.toast_title = shown.status.clone();
                shown.toast_copy = stage.text();
                shown.toast_key = format!("update:{}", release.version());
                shown.in_progress = true;
                shown.progress_permille = stage.permille();
            }
            Self::InstallFailed(release, message) => {
                shown.toast_title = tr!("Не удалось обновить").into();
                shown.toast_copy = message.clone();
                shown.toast_key = format!("failed:{}", release.version());
                shown.failed = true;
            }
            _ => {}
        }
        shown
    }

    /// The Settings row (status, detail, action) of each state.
    fn row_presentation(&self, runtime_idle: bool, configured: bool) -> UpdatePresentation {
        match self {
            Self::Unconfigured => UpdatePresentation {
                status: format!("MULTICORE {}", env!("CARGO_PKG_VERSION")),
                detail: tr!("Канал обновлений задаётся при сборке").into(),
                action_label: String::new(),
                action: UpdateAction::None,
                action_enabled: false,
                available: false,
                busy: false,
                ..UpdatePresentation::default()
            },
            Self::Checking => UpdatePresentation {
                status: tr!("Проверяем обновления…").into(),
                detail: tr!("Установлена версия {}", env!("CARGO_PKG_VERSION")),
                action_label: tr!("Проверяем…").into(),
                action: UpdateAction::None,
                action_enabled: false,
                available: false,
                busy: true,
                ..UpdatePresentation::default()
            },
            Self::Deferred => UpdatePresentation {
                status: format!("MULTICORE {}", env!("CARGO_PKG_VERSION")),
                detail: tr!("Проверим обновления чуть позже").into(),
                action_label: tr!("Проверить").into(),
                action: UpdateAction::Check,
                action_enabled: true,
                available: false,
                busy: false,
                ..UpdatePresentation::default()
            },
            Self::Current => UpdatePresentation {
                status: tr!("Установлена актуальная версия").into(),
                detail: format!("MULTICORE {}", env!("CARGO_PKG_VERSION")),
                action_label: tr!("Проверить").into(),
                action: UpdateAction::Check,
                action_enabled: true,
                available: false,
                busy: false,
                ..UpdatePresentation::default()
            },
            Self::Available(release) => UpdatePresentation {
                status: tr!("Доступна версия {}", release.version()),
                detail: if runtime_idle {
                    replacement_detail().into()
                } else {
                    tr!("После загрузки туннель будет остановлен, а клиент перезапущен").into()
                },
                action_label: tr!("Установить").into(),
                action: UpdateAction::Install,
                action_enabled: true,
                available: true,
                busy: false,
                ..UpdatePresentation::default()
            },
            Self::Managed(release) => UpdatePresentation {
                status: tr!("Доступна версия {}", release.version()),
                detail: tr!("Обновите пакет через менеджер пакетов").into(),
                action_label: String::new(),
                action: UpdateAction::None,
                action_enabled: false,
                available: true,
                busy: false,
                ..UpdatePresentation::default()
            },
            Self::Updating(release, stage) => UpdatePresentation {
                status: tr!("Обновление до {}", release.version()),
                detail: stage.text(),
                action_label: String::new(),
                action: UpdateAction::None,
                action_enabled: false,
                available: true,
                busy: true,
                ..UpdatePresentation::default()
            },
            Self::InstallFailed(release, message) => UpdatePresentation {
                status: tr!("Не удалось установить версию {}", release.version()),
                detail: message.clone(),
                action_label: tr!("Повторить").into(),
                action: UpdateAction::Install,
                action_enabled: true,
                available: true,
                busy: false,
                ..UpdatePresentation::default()
            },
            Self::Error(message) => UpdatePresentation {
                status: tr!("Не удалось проверить обновления").into(),
                detail: message.clone(),
                action_label: if configured {
                    tr!("Повторить").into()
                } else {
                    String::new()
                },
                action: if configured {
                    UpdateAction::Check
                } else {
                    UpdateAction::None
                },
                action_enabled: configured,
                available: false,
                busy: false,
                ..UpdatePresentation::default()
            },
        }
    }

    pub(crate) fn available_release(&self) -> Option<GitHubRelease> {
        match self {
            Self::Available(release) | Self::InstallFailed(release, _) => Some(release.clone()),
            _ => None,
        }
    }

    /// Starts an update run of the offered release (also a retry after a failure) and
    /// returns it; `None` when nothing is offered or a run is already going (a second press).
    pub(crate) fn begin_update(&mut self) -> Option<GitHubRelease> {
        let release = self.available_release()?;
        let size = release.asset().size();
        *self = Self::Updating(
            release.clone(),
            UpdateStage::Downloading(DownloadProgress {
                received: 0,
                total: size,
            }),
        );
        Some(release)
    }

    /// Moves the running update to `stage` when that is forward; whether anything changed.
    pub(crate) fn advance(&mut self, stage: UpdateStage) -> bool {
        match self {
            Self::Updating(_, current) if current.precedes(stage) => {
                *current = stage;
                true
            }
            _ => false,
        }
    }
}

pub(crate) fn configured_repository() -> Result<Option<Repository>, String> {
    let Some(value) = option_env!("MULTICORE_UPDATE_REPOSITORY") else {
        return Ok(None);
    };
    Repository::parse(value)
        .map(Some)
        .map_err(|_| tr!("В сборке указан неверный GitHub-репозиторий").into())
}

/// Why `release.txt` gave no version: the check falls back to the releases API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HintError {
    /// No such file (404) or another HTTP failure.
    Missing,
    /// Not one strict `MAJOR.MINOR.PATCH` version, or too long.
    Invalid,
    Unreachable,
}

/// A releases API failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ApiError {
    /// 403/429: the shared unauthenticated limit is used up; try later, quietly.
    RateLimited,
    Failed(String),
}

/// What one check learned.
#[derive(Clone, Debug)]
pub(crate) enum CheckOutcome {
    State(UpdateState),
    /// Nothing new (the API was rate limited, or its fallback is not due yet): keep the
    /// state the app already shows.
    Unchanged,
}

/// `release.txt`: exactly one strict version (`1.1.4`, surrounding whitespace allowed).
pub(crate) fn parse_release_hint(body: &[u8]) -> Result<Version, HintError> {
    if body.len() > RELEASE_HINT_LIMIT {
        return Err(HintError::Invalid);
    }
    let text = std::str::from_utf8(body)
        .map_err(|_| HintError::Invalid)?
        .trim();
    if text.starts_with('v') {
        return Err(HintError::Invalid);
    }
    Version::parse(text).map_err(|_| HintError::Invalid)
}

/// One update check. `release.txt` is only a hint: when it names a newer version than this
/// build, the releases API supplies the release (asset, size, digest) exactly as before; an
/// equal or older version answers without touching the API. Without a usable hint the API is
/// asked at most every `API_FALLBACK_INTERVAL`, unless the user asked (`manual`). A release
/// already offered (`known`) is not fetched again for the same version.
pub(crate) fn check_latest(manual: bool, known: Option<Version>) -> CheckOutcome {
    let repository = match configured_repository() {
        Ok(Some(repository)) => repository,
        Ok(None) => {
            return CheckOutcome::State(UpdateState::Error(
                tr!("Канал обновлений не настроен").into(),
            ));
        }
        Err(error) => return CheckOutcome::State(UpdateState::Error(error)),
    };
    let Ok(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        return CheckOutcome::State(UpdateState::Error(
            tr!("Версия клиента собрана неверно").into(),
        ));
    };
    let hint = fetch_release_hint(&repository);
    plan_check(
        &current,
        hint,
        known,
        manual,
        &LAST_API_CHECK,
        Instant::now(),
        || fetch_latest_release(&repository),
    )
}

pub(crate) fn plan_check(
    current: &Version,
    hint: Result<Version, HintError>,
    known: Option<Version>,
    manual: bool,
    last_api_check: &Mutex<Option<Instant>>,
    now: Instant,
    fetch_release: impl FnOnce() -> Result<GitHubRelease, ApiError>,
) -> CheckOutcome {
    match hint {
        Ok(latest) if latest <= *current => return CheckOutcome::State(UpdateState::Current),
        Ok(latest) if known == Some(latest) => return CheckOutcome::Unchanged,
        Ok(_) => {}
        Err(_) => {
            let last = *last_api_check.lock().expect("update clock lock");
            let due = last
                .is_none_or(|last| now.saturating_duration_since(last) >= API_FALLBACK_INTERVAL);
            if !manual && !due {
                return CheckOutcome::Unchanged;
            }
        }
    }
    *last_api_check.lock().expect("update clock lock") = Some(now);
    match fetch_release() {
        Ok(release) => CheckOutcome::State(match release.decision(current) {
            ReleaseDecision::Current => UpdateState::Current,
            ReleaseDecision::UpdateAvailable => available_state(release),
        }),
        Err(ApiError::RateLimited) => CheckOutcome::Unchanged,
        Err(ApiError::Failed(message)) => CheckOutcome::State(UpdateState::Error(message)),
    }
}

fn update_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()
        .map_err(|_| tr!("Не удалось создать защищённое соединение").to_owned())
}

/// The pinned repository's `release.txt` (raw.githubusercontent.com caches it ~5 min).
fn fetch_release_hint(repository: &Repository) -> Result<Version, HintError> {
    let client = update_client(Duration::from_secs(8)).map_err(|_| HintError::Unreachable)?;
    let response = client
        .get(repository.release_hint_url())
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| HintError::Unreachable)?;
    if !response.status().is_success() {
        return Err(HintError::Missing);
    }
    let mut body = Vec::new();
    response
        .take(RELEASE_HINT_LIMIT as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| HintError::Unreachable)?;
    parse_release_hint(&body)
}

fn fetch_latest_release(repository: &Repository) -> Result<GitHubRelease, ApiError> {
    let body = fetch_latest_release_body_checked(repository)?;
    #[cfg(not(target_os = "linux"))]
    let release = GitHubRelease::parse_for_repository(repository, &body);
    #[cfg(target_os = "linux")]
    let release = GitHubRelease::parse_appimage_for_repository(repository, &body);
    release.map_err(|_| ApiError::Failed(tr!("Релиз не соответствует контракту MULTICORE").into()))
}

/// Whether an API answer is GitHub's rate limit rather than a real failure.
pub(crate) fn is_rate_limited(status: u16) -> bool {
    matches!(status, 403 | 429)
}

/// The latest-release JSON of the pinned repository, bounded in size (the service-install card).
#[cfg(windows)]
pub(crate) fn fetch_latest_release_body(repository: &Repository) -> Result<Vec<u8>, String> {
    fetch_latest_release_body_checked(repository).map_err(|error| match error {
        ApiError::RateLimited => {
            tr!("GitHub временно ограничил запросы. Повторите позже").to_owned()
        }
        ApiError::Failed(message) => message,
    })
}

fn fetch_latest_release_body_checked(repository: &Repository) -> Result<Vec<u8>, ApiError> {
    let client = update_client(Duration::from_secs(12)).map_err(ApiError::Failed)?;
    let mut response = client
        .get(repository.latest_release_api_url())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| ApiError::Failed(tr!("GitHub сейчас недоступен").to_owned()))?;
    if is_rate_limited(response.status().as_u16()) {
        return Err(ApiError::RateLimited);
    }
    if !response.status().is_success() {
        return Err(ApiError::Failed(tr!(
            "GitHub ответил HTTP {}",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > RELEASE_BODY_LIMIT as u64)
    {
        return Err(ApiError::Failed(tr!("Ответ GitHub слишком большой").into()));
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(RELEASE_BODY_LIMIT as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| ApiError::Failed(tr!("Не удалось прочитать ответ GitHub").to_owned()))?;
    if body.len() > RELEASE_BODY_LIMIT {
        return Err(ApiError::Failed(tr!("Ответ GitHub слишком большой").into()));
    }
    Ok(body)
}

#[cfg(not(target_os = "linux"))]
fn available_state(release: GitHubRelease) -> UpdateState {
    UpdateState::Available(release)
}

#[cfg(target_os = "linux")]
fn available_state(release: GitHubRelease) -> UpdateState {
    match linux_install() {
        LinuxInstall::AppImage(_) => UpdateState::Available(release),
        LinuxInstall::Package => UpdateState::Managed(release),
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinuxInstall {
    AppImage(PathBuf),
    Package,
}

#[cfg(target_os = "linux")]
pub(crate) fn linux_install_from(appimage: Option<&std::ffi::OsStr>) -> LinuxInstall {
    appimage
        .map(PathBuf::from)
        .filter(|image| image.is_absolute() && image.is_file())
        .map_or(LinuxInstall::Package, LinuxInstall::AppImage)
}

#[cfg(target_os = "linux")]
fn linux_install() -> LinuxInstall {
    linux_install_from(std::env::var_os("APPIMAGE").as_deref())
}

#[cfg(target_os = "linux")]
pub(crate) fn install_appimage(image: &Path, staged: &Path, backup: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(staged, fs::Permissions::from_mode(0o755))?;
    match fs::remove_file(backup) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    if fs::hard_link(image, backup).is_err() {
        fs::copy(image, backup)?;
    }
    fs::rename(staged, image)
}

/// A release downloaded and verified (size and SHA-256 from the release) while the tunnel
/// stays up, waiting to be applied once the client has disconnected.
#[derive(Debug)]
pub(crate) struct StagedUpdate {
    path: PathBuf,
}

impl StagedUpdate {
    #[cfg(test)]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Checks the staged file again right before it is applied: it waited in a user-writable
    /// directory while the tunnel was being stopped.
    pub(crate) fn reverify(&self, release: &GitHubRelease) -> Result<(), String> {
        verify_archive_file(
            &self.path,
            release.asset().size(),
            release.asset().sha256_hex(),
        )
        .map_err(|_| {
            tr!("Загруженное обновление изменилось до установки — повторите загрузку").into()
        })
    }
}

/// The AppImage being replaced, the hidden download next to it, and the kept previous image.
#[cfg(target_os = "linux")]
fn appimage_paths() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let LinuxInstall::AppImage(image) = linux_install() else {
        return Err(tr!("Обновите MULTICORE через менеджер пакетов").into());
    };
    let directory = image
        .parent()
        .ok_or_else(|| tr!("Некорректный путь AppImage").to_owned())?;
    let name = image
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| tr!("Некорректное имя AppImage").to_owned())?;
    let staged = directory.join(format!(".{name}.update"));
    let backup = directory.join(format!("{name}.old"));
    Ok((image, staged, backup))
}

/// Downloads and verifies `release` without touching the installed AppImage, so the tunnel can
/// stay up meanwhile. `report` hears the download's progress, then `Verifying`.
#[cfg(target_os = "linux")]
pub(crate) fn download_and_verify(
    release: &GitHubRelease,
    mut report: impl FnMut(UpdateStage),
) -> Result<StagedUpdate, String> {
    let (_, staged, _) = appimage_paths()?;
    if staged.exists() {
        fs::remove_file(&staged)
            .map_err(|_| tr!("Не удалось очистить прошлую загрузку").to_owned())?;
    }
    download_release_to(release, &staged, |progress| {
        report(UpdateStage::Downloading(progress));
    })?;
    report(UpdateStage::Verifying);
    if verify_archive_file(
        &staged,
        release.asset().size(),
        release.asset().sha256_hex(),
    )
    .is_err()
    {
        let _ = fs::remove_file(&staged);
        return Err(tr!("Загруженный AppImage не прошёл SHA-256 проверку").into());
    }
    Ok(StagedUpdate { path: staged })
}

/// Swaps in the staged AppImage and relaunches it once this process has exited.
#[cfg(target_os = "linux")]
pub(crate) fn start_apply(release: &GitHubRelease, staged: &StagedUpdate) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    if let Err(error) = staged.reverify(release) {
        let _ = fs::remove_file(&staged.path);
        return Err(error);
    }
    let (image, _, backup) = appimage_paths()?;
    install_appimage(&image, &staged.path, &backup)
        .map_err(|_| tr!("Не удалось заменить файл AppImage").to_owned())?;
    Command::new("/bin/sh")
        .arg("-c")
        .arg("while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; exec \"$2\"")
        .arg("multicore-relaunch")
        .arg(std::process::id().to_string())
        .arg(&image)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| tr!("Не удалось перезапустить MULTICORE").to_owned())?;
    Ok(())
}

/// Downloads and verifies `release` into the user's update directory without touching the
/// install, so the tunnel can stay up meanwhile. `report` hears the download's progress, then
/// `Verifying`.
#[cfg(not(target_os = "linux"))]
pub(crate) fn download_and_verify(
    release: &GitHubRelease,
    mut report: impl FnMut(UpdateStage),
) -> Result<StagedUpdate, String> {
    let update_dir = update_directory()?;
    fs::create_dir_all(&update_dir)
        .map_err(|_| tr!("Не удалось создать папку обновлений").to_owned())?;
    let partial = update_dir.join(format!("multicore-{}.zip.partial", release.version()));
    let archive = update_dir.join(format!("multicore-{}.zip", release.version()));
    for stale in [&partial, &archive] {
        if stale.exists() {
            fs::remove_file(stale)
                .map_err(|_| tr!("Не удалось очистить прошлую загрузку").to_owned())?;
        }
    }

    download_release_to(release, &partial, |progress| {
        report(UpdateStage::Downloading(progress));
    })?;
    report(UpdateStage::Verifying);
    verify_archive_file(
        &partial,
        release.asset().size(),
        release.asset().sha256_hex(),
    )
    .map_err(|_| tr!("Загруженный архив не прошёл SHA-256 проверку").to_owned())?;
    fs::rename(&partial, &archive)
        .map_err(|_| tr!("Не удалось зафиксировать загруженный архив").to_owned())?;
    Ok(StagedUpdate { path: archive })
}

/// Starts the update helper on the staged archive; it waits for this process to exit, swaps
/// the install (verifying the archive once more) and relaunches MultiCore.
#[cfg(not(target_os = "linux"))]
pub(crate) fn start_apply(release: &GitHubRelease, staged: &StagedUpdate) -> Result<(), String> {
    staged.reverify(release)?;
    let update_dir = update_directory()?;
    let current_exe = std::env::current_exe()
        .map_err(|_| tr!("Не удалось определить путь MULTICORE").to_owned())?;
    let target = current_exe
        .parent()
        .ok_or_else(|| tr!("Некорректный путь MULTICORE").to_owned())?
        .to_path_buf();
    let helper_source = target.join("runtime").join("multicore-updater.exe");
    if !helper_source.is_file() {
        return Err(
            tr!("В установке нет программы обновления (runtime/multicore-updater.exe)").into(),
        );
    }
    remove_stale_helpers(&update_dir);
    // The PID keeps a still-running helper from an earlier attempt at the
    // same version (it can wait for ~90 s) from blocking this copy.
    let helper = update_dir.join(format!(
        "{HELPER_PREFIX}{}-{}.exe",
        release.version(),
        std::process::id()
    ));
    fs::copy(&helper_source, &helper)
        .map_err(|_| tr!("Не удалось подготовить программу обновления").to_owned())?;

    let mut command = Command::new(helper);
    command
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .arg("--archive")
        .arg(&staged.path)
        .arg("--target")
        .arg(&target)
        .arg("--size")
        .arg(release.asset().size().to_string())
        .arg("--sha256")
        .arg(release.asset().sha256_hex())
        .arg("--version")
        .arg(release.version().to_string())
        .current_dir(&update_dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map_err(|_| tr!("Не удалось запустить программу обновления").to_owned())?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
const HELPER_PREFIX: &str = "multicore-apply-";

/// Best-effort: copies from earlier attempts are no longer needed; one that
/// is still running is locked and simply stays until the next attempt.
#[cfg(not(target_os = "linux"))]
fn remove_stale_helpers(update_dir: &Path) {
    let Ok(entries) = fs::read_dir(update_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let is_helper = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(HELPER_PREFIX) && name.ends_with(".exe"));
        if is_helper {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Outcome the Windows update helper writes after it tried to swap in a new
/// version (see `write_result` in apps/multicore-updater). Only a failure is
/// kept: the helper relaunches the *old* client then, and without reading
/// this the user just sees the same "update available" banner again with no
/// explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstallFailure {
    pub version: String,
    pub message: String,
}

#[cfg(not(target_os = "linux"))]
const RESULT_FILE: &str = "result.json";

static PENDING_INSTALL_FAILURE: std::sync::Mutex<Option<InstallFailure>> =
    std::sync::Mutex::new(None);

/// Reads and deletes the helper's result file once at startup, remembering a
/// failure for the next update check (see `with_pending_install_failure`).
pub(crate) fn load_install_result() {
    #[cfg(not(target_os = "linux"))]
    if let Ok(update_dir) = update_directory()
        && let Some(failure) = take_install_failure_in(&update_dir)
    {
        *PENDING_INSTALL_FAILURE
            .lock()
            .expect("pending install failure lock") = Some(failure);
    }
}

#[cfg(not(target_os = "linux"))]
fn take_install_failure_in(update_dir: &Path) -> Option<InstallFailure> {
    #[derive(serde::Deserialize)]
    struct HelperResult {
        success: bool,
        version: String,
        message: String,
    }
    let path = update_dir.join(RESULT_FILE);
    let bytes = fs::read(&path).ok()?;
    let _ = fs::remove_file(&path);
    let result: HelperResult = serde_json::from_slice(&bytes).ok()?;
    (!result.success).then(|| InstallFailure {
        version: result.version,
        message: result.message.chars().take(300).collect(),
    })
}

/// Turns a fresh check result into `InstallFailed` when the last install of
/// that same release failed, so the banner explains why it is still offered.
pub(crate) fn with_pending_install_failure(state: UpdateState) -> UpdateState {
    let mut pending = PENDING_INSTALL_FAILURE
        .lock()
        .expect("pending install failure lock");
    merge_install_failure(state, &mut pending)
}

fn merge_install_failure(state: UpdateState, pending: &mut Option<InstallFailure>) -> UpdateState {
    match state {
        UpdateState::Available(release) => match pending.take() {
            Some(failure) if failure.version == release.version().to_string() => {
                UpdateState::InstallFailed(release, failure.message)
            }
            _ => UpdateState::Available(release),
        },
        UpdateState::Current => {
            *pending = None;
            UpdateState::Current
        }
        // A failed check keeps the install failure for the next one.
        other => other,
    }
}

/// Downloads the release's asset from GitHub to `destination`; `progress` hears every whole
/// percent (see `receive_exact`).
pub(crate) fn download_release_to(
    release: &GitHubRelease,
    destination: &Path,
    progress: impl FnMut(DownloadProgress),
) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(180))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let allowed = attempt.url().scheme() == "https"
                && matches!(
                    attempt.url().host_str(),
                    Some("github.com")
                        | Some("release-assets.githubusercontent.com")
                        | Some("objects.githubusercontent.com")
                );
            if allowed && attempt.previous().len() < 5 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|_| tr!("Не удалось создать соединение для загрузки").to_owned())?;
    download_to(
        &client,
        release.asset().download_url(),
        release.asset().size(),
        destination,
        progress,
    )
}

/// Fetches `url`, which must be exactly `size` bytes (from the verified release, never from
/// the server), into `destination`.
fn download_to(
    client: &reqwest::blocking::Client,
    url: &str,
    size: u64,
    destination: &Path,
    progress: impl FnMut(DownloadProgress),
) -> Result<(), String> {
    let mut response = client
        .get(url)
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| tr!("Не удалось загрузить обновление").to_owned())?;
    if !response.status().is_success() {
        return Err(tr!(
            "Загрузка обновления вернула HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != size)
    {
        return Err(tr!("Размер обновления не совпал с релизом").into());
    }
    let mut output = File::create(destination)
        .map_err(|_| tr!("Не удалось создать временный архив").to_owned())?;
    receive_exact(&mut response, &mut output, size, progress).map_err(|error| {
        match error {
            DownloadError::Read => tr!("Загрузка обновления прервалась"),
            DownloadError::Write => tr!("Не удалось записать обновление"),
            DownloadError::Short => tr!("Обновление загрузилось не полностью"),
            DownloadError::Long => tr!("Обновление больше заявленного размера"),
        }
        .to_owned()
    })
}

/// Left by the app right before it quits for an update: the version it is about to become.
/// The next start reads it once (`take_update_confirmation`) to say «Обновлено до …».
const UPDATED_MARKER: &str = "updated-to";

fn updates_root() -> Option<PathBuf> {
    crate::data_root::user_data_root()
        .ok()
        .map(|root| root.join("updates"))
}

/// Notes that this process quits so that `version` replaces it (see `UPDATED_MARKER`).
pub(crate) fn remember_restart_into(version: &Version) {
    if let Some(directory) = updates_root() {
        let _ = remember_restart_into_in(&directory, version);
    }
}

fn remember_restart_into_in(directory: &Path, version: &Version) -> std::io::Result<()> {
    fs::create_dir_all(directory)?;
    fs::write(directory.join(UPDATED_MARKER), version.to_string())
}

/// Once after an update: the version this build was updated to, when it is this build (an
/// update that failed and relaunched the old version says nothing here; its helper's result
/// explains it instead). The marker is removed either way.
pub(crate) fn take_update_confirmation() -> Option<String> {
    take_update_confirmation_in(&updates_root()?, env!("CARGO_PKG_VERSION"))
}

fn take_update_confirmation_in(directory: &Path, current: &str) -> Option<String> {
    let marker = directory.join(UPDATED_MARKER);
    let mut content = String::new();
    File::open(&marker)
        .ok()?
        .take(RELEASE_HINT_LIMIT as u64)
        .read_to_string(&mut content)
        .ok();
    let _ = fs::remove_file(&marker);
    let version = content.trim();
    (version == current).then(|| version.to_owned())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn update_directory() -> Result<PathBuf, String> {
    crate::data_root::user_data_root()
        .map(|root| root.join("updates"))
        .map_err(|_| tr!("Каталог данных MULTICORE недоступен").to_owned())
}

/// Marker the Windows updater waits for before discarding the previous version.
/// Must match `STARTED_MARKER` in `apps/multicore-updater`.
#[cfg(not(target_os = "linux"))]
const STARTED_MARKER: &str = "started-ok";
#[cfg(not(target_os = "linux"))]
const BACKUP_PREFIX: &str = ".multicore-previous-";

/// Confirm that this build started successfully (window and daemon are up).
///
/// The updater keeps the previous-version backup until this marker names the
/// new version. If the marker already named this version before (an earlier
/// confirmed start), leftover backups are stale and are removed.
#[cfg(not(target_os = "linux"))]
pub(crate) fn confirm_started() {
    let Ok(update_dir) = update_directory() else {
        return;
    };
    let Some(install_parent) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(Path::to_path_buf))
    else {
        return;
    };
    if let Err(error) = confirm_started_in(&update_dir, &install_parent, env!("CARGO_PKG_VERSION"))
    {
        eprintln!("update start confirmation failed: {error}");
    }
}

/// Whether this build has confirmed its start (see `confirm_started`) at or
/// after `since`; used by the renderer-fallback launcher to know when the
/// restarted instance has taken over.
#[cfg(not(target_os = "linux"))]
pub(crate) fn start_confirmed_since(since: std::time::SystemTime) -> bool {
    let Ok(update_dir) = update_directory() else {
        return false;
    };
    let marker = update_dir.join(STARTED_MARKER);
    fs::metadata(&marker)
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified >= since)
        && fs::read_to_string(&marker)
            .is_ok_and(|content| content.trim() == env!("CARGO_PKG_VERSION"))
}

#[cfg(not(target_os = "linux"))]
fn confirm_started_in(
    update_dir: &Path,
    install_parent: &Path,
    version: &str,
) -> std::io::Result<()> {
    let marker = update_dir.join(STARTED_MARKER);
    let previously_confirmed =
        fs::read_to_string(&marker).is_ok_and(|content| content.trim() == version);
    if previously_confirmed && let Ok(entries) = fs::read_dir(install_parent) {
        for entry in entries.flatten() {
            let stale = entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(BACKUP_PREFIX));
            if stale {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    fs::create_dir_all(update_dir)?;
    let temporary = update_dir.join(format!("{STARTED_MARKER}.tmp"));
    fs::write(&temporary, version)?;
    fs::rename(temporary, marker)
}

#[cfg(test)]
mod tests {
    use super::{UpdateAction, UpdateState};
    use multicore_release::{GitHubRelease, WindowsArch};

    fn release() -> GitHubRelease {
        GitHubRelease::parse_for_arch(br#"{
            "tag_name":"v0.2.0",
            "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
            "assets":[{
                "name":"multicore-windows-x64.zip",
                "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
                "size":1234,
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }"#, WindowsArch::X64).unwrap()
    }

    fn version(value: &str) -> multicore_release::Version {
        multicore_release::Version::parse(value).unwrap()
    }

    #[test]
    fn release_hint_is_one_strict_bounded_version() {
        use super::{HintError, parse_release_hint};
        assert_eq!(parse_release_hint(b"1.1.4\n"), Ok(version("1.1.4")));
        assert_eq!(parse_release_hint(b"  2.0.0 \r\n"), Ok(version("2.0.0")));
        for garbage in [
            &b""[..],
            b"v1.1.4\n",
            b"1.1\n",
            b"1.1.4.5",
            b"1.01.4",
            b"latest",
            b"<html>404</html>",
            b"\xff\xfe1.1.4",
        ] {
            assert_eq!(
                parse_release_hint(garbage),
                Err(HintError::Invalid),
                "{garbage:?}"
            );
        }
        let oversized = format!("1.1.4{}", " ".repeat(64));
        assert_eq!(
            parse_release_hint(oversized.as_bytes()),
            Err(HintError::Invalid)
        );
    }

    #[test]
    fn the_api_is_asked_only_when_the_hint_names_a_newer_version() {
        use super::{CheckOutcome, HintError, plan_check};
        use std::cell::Cell;
        use std::sync::Mutex;
        use std::time::Instant;
        let current = version("0.1.0");
        let now = Instant::now();
        let clock = Mutex::new(None);
        let calls = Cell::new(0);
        let api = || {
            calls.set(calls.get() + 1);
            Ok(release())
        };

        // Equal or older: up to date, no API call.
        for hint in ["0.1.0", "0.0.9"] {
            let outcome = plan_check(&current, Ok(version(hint)), None, false, &clock, now, api);
            assert!(matches!(outcome, CheckOutcome::State(UpdateState::Current)));
        }
        assert_eq!(calls.get(), 0);
        assert_eq!(*clock.lock().unwrap(), None);

        // Newer: the release comes from the API, with all its checks.
        let outcome = plan_check(
            &current,
            Ok(version("0.2.0")),
            None,
            false,
            &clock,
            now,
            api,
        );
        assert!(matches!(
            outcome,
            CheckOutcome::State(UpdateState::Available(_) | UpdateState::Managed(_))
        ));
        assert_eq!(calls.get(), 1);

        // The same newer version already offered: nothing to fetch again.
        let outcome = plan_check(
            &current,
            Ok(version("0.2.0")),
            Some(version("0.2.0")),
            false,
            &clock,
            now,
            api,
        );
        assert!(matches!(outcome, CheckOutcome::Unchanged));
        assert_eq!(calls.get(), 1);

        // Garbage or a missing file: the hint is not trusted for anything.
        let fresh = Mutex::new(None);
        let outcome = plan_check(
            &current,
            Err(HintError::Invalid),
            None,
            false,
            &fresh,
            now,
            api,
        );
        assert!(matches!(
            outcome,
            CheckOutcome::State(UpdateState::Available(_) | UpdateState::Managed(_))
        ));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn without_a_hint_the_api_runs_on_its_own_four_hour_cadence() {
        use super::{API_FALLBACK_INTERVAL, CheckOutcome, HintError, plan_check};
        use std::cell::Cell;
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        let current = version("0.1.0");
        let start = Instant::now();
        let clock = Mutex::new(None);
        let calls = Cell::new(0);
        let api = || {
            calls.set(calls.get() + 1);
            Ok(release())
        };
        let missing = || Err(HintError::Missing);

        // First check (startup): the API answers.
        let _ = plan_check(&current, missing(), None, false, &clock, start, api);
        assert_eq!(calls.get(), 1);
        // Hourly checks within 4 h: nothing new, the API is left alone.
        for hours in 1..4 {
            let at = start + Duration::from_secs(hours * 3_600);
            let outcome = plan_check(&current, missing(), None, false, &clock, at, api);
            assert!(matches!(outcome, CheckOutcome::Unchanged));
        }
        assert_eq!(calls.get(), 1);
        // A user's own «Проверить» does not wait.
        let _ = plan_check(
            &current,
            missing(),
            None,
            true,
            &clock,
            start + Duration::from_secs(3_700),
            api,
        );
        assert_eq!(calls.get(), 2);
        // 4 h after that last API call it is due again.
        let at = start + Duration::from_secs(3_700) + API_FALLBACK_INTERVAL;
        let _ = plan_check(&current, missing(), None, false, &clock, at, api);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_rate_limited_api_is_quietly_tried_later() {
        use super::{ApiError, CheckOutcome, is_rate_limited, plan_check};
        use std::sync::Mutex;
        use std::time::Instant;
        assert!(is_rate_limited(403));
        assert!(is_rate_limited(429));
        assert!(!is_rate_limited(404));
        assert!(!is_rate_limited(500));
        let current = version("0.1.0");
        let clock = Mutex::new(None);
        let outcome = plan_check(
            &current,
            Ok(version("0.2.0")),
            None,
            false,
            &clock,
            Instant::now(),
            || Err(ApiError::RateLimited),
        );
        assert!(matches!(outcome, CheckOutcome::Unchanged));
        let outcome = plan_check(
            &current,
            Ok(version("0.2.0")),
            None,
            false,
            &clock,
            Instant::now(),
            || Err(ApiError::Failed("GitHub ответил HTTP 500".into())),
        );
        assert!(matches!(
            outcome,
            CheckOutcome::State(UpdateState::Error(_))
        ));
        // Deferred is a quiet state with a manual retry, never an error.
        let deferred = UpdateState::Deferred.presentation(true);
        assert!(!deferred.available);
        assert_eq!(deferred.action, UpdateAction::Check);
        assert!(!deferred.status.contains("Не удалось"));
    }

    #[test]
    fn available_update_can_request_a_managed_restart_while_connected() {
        let state = UpdateState::Available(release());
        let idle = state.presentation(true);
        assert_eq!(idle.action, UpdateAction::Install);
        assert!(idle.action_enabled);
        assert!(idle.available);

        let connected = state.presentation(false);
        assert_eq!(connected.action, UpdateAction::Install);
        assert!(connected.action_enabled);
        assert!(connected.detail.contains("перезапущен"));
        assert!(
            connected.detail.contains("После загрузки"),
            "the tunnel is stopped only once the download is ready: {}",
            connected.detail
        );
    }

    fn release_of(content: &[u8]) -> GitHubRelease {
        use sha2::{Digest, Sha256};
        let body = format!(
            r#"{{
            "tag_name":"v0.2.0",
            "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
            "assets":[{{
                "name":"multicore-windows-x64.zip",
                "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
                "size":{},
                "digest":"sha256:{:x}"
            }}]
        }}"#,
            content.len(),
            Sha256::digest(content)
        );
        GitHubRelease::parse_for_arch(body.as_bytes(), WindowsArch::X64).unwrap()
    }

    #[test]
    fn a_staged_update_is_checked_again_before_it_is_applied() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("multicore-0.2.0.zip");
        std::fs::write(&path, b"verified package").unwrap();
        let release = release_of(b"verified package");
        let staged = super::StagedUpdate::at(path.clone());
        assert_eq!(staged.reverify(&release), Ok(()));
        // It waited in the user's update directory while the tunnel was stopped.
        std::fs::write(&path, b"tampered package").unwrap();
        assert!(staged.reverify(&release).is_err());
        assert!(
            super::start_apply(&release, &staged).is_err(),
            "a changed package is never handed to the installer"
        );
        // Linux drops a tampered download itself; either way a missing file is rejected.
        let _ = std::fs::remove_file(&path);
        assert!(staged.reverify(&release).is_err());
    }

    #[test]
    fn applying_is_separate_from_downloading_so_the_tunnel_can_stay_up_meanwhile() {
        let source = include_str!("updater.rs").replace("\r\n", "\n");
        assert!(!source.contains(&format!("fn {}(", "download_and_start_apply")));
        let apply = source
            .split("pub(crate) fn start_apply(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("start_apply");
        assert!(
            apply.contains("staged.reverify(release)"),
            "start_apply re-checks the staged file first"
        );
        assert!(
            !apply.contains("download_release_to("),
            "applying never downloads"
        );
    }

    #[test]
    fn available_install_is_labelled_install_not_refresh() {
        // "Обновить" sat right above the subscription's own "Обновить"
        // button, and users confused the two.
        let ui = UpdateState::Available(release()).presentation(true);
        assert_eq!(ui.action_label, "Установить");
        assert!(!ui.detail.contains("portable"));
    }

    #[test]
    fn install_failure_keeps_the_banner_with_the_reason_and_a_retry() {
        let state = UpdateState::InstallFailed(release(), "Файл занят".into());
        let ui = state.presentation(true);
        assert!(ui.available, "the home banner must stay visible");
        assert_eq!(ui.action, UpdateAction::Install);
        assert!(ui.action_enabled);
        assert!(ui.status.contains("0.2.0"));
        assert_eq!(ui.detail, "Файл занят");
        assert!(state.available_release().is_some());
    }

    #[test]
    fn a_pending_install_failure_marks_only_the_same_release() {
        let failure = || {
            Some(super::InstallFailure {
                version: "0.2.0".into(),
                message: "swap failed".into(),
            })
        };

        let mut pending = failure();
        match super::merge_install_failure(UpdateState::Available(release()), &mut pending) {
            UpdateState::InstallFailed(_, message) => assert_eq!(message, "swap failed"),
            other => panic!("expected InstallFailed, got {other:?}"),
        }
        assert!(pending.is_none(), "shown once");

        let mut pending = Some(super::InstallFailure {
            version: "0.1.9".into(),
            message: "old".into(),
        });
        assert!(matches!(
            super::merge_install_failure(UpdateState::Available(release()), &mut pending),
            UpdateState::Available(_)
        ));

        let mut pending = failure();
        assert!(matches!(
            super::merge_install_failure(UpdateState::Error("offline".into()), &mut pending),
            UpdateState::Error(_)
        ));
        assert!(
            pending.is_some(),
            "a failed check keeps it for the next one"
        );
        assert!(matches!(
            super::merge_install_failure(UpdateState::Current, &mut pending),
            UpdateState::Current
        ));
        assert!(pending.is_none());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn helper_result_file_is_read_once_and_only_failures_are_kept() {
        let temp = tempfile::tempdir().unwrap();
        let result = temp.path().join("result.json");

        assert_eq!(super::take_install_failure_in(temp.path()), None);

        std::fs::write(
            &result,
            br#"{"success":false,"version":"0.2.0","message":"Access denied","timestamp_unix":1}"#,
        )
        .unwrap();
        assert_eq!(
            super::take_install_failure_in(temp.path()),
            Some(super::InstallFailure {
                version: "0.2.0".into(),
                message: "Access denied".into(),
            })
        );
        assert!(!result.exists(), "the result is consumed");

        std::fs::write(
            &result,
            br#"{"success":true,"version":"0.2.0","message":"ok","timestamp_unix":1}"#,
        )
        .unwrap();
        assert_eq!(super::take_install_failure_in(temp.path()), None);
        assert!(!result.exists());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn stale_helper_copies_are_removed_but_other_files_are_kept() {
        let temp = tempfile::tempdir().unwrap();
        let helper = temp.path().join("multicore-apply-0.2.0-123.exe");
        let archive = temp.path().join("multicore-0.2.0.zip");
        std::fs::write(&helper, b"x").unwrap();
        std::fs::write(&archive, b"x").unwrap();
        super::remove_stale_helpers(temp.path());
        assert!(!helper.exists());
        assert!(archive.exists());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn start_confirmation_writes_marker_and_cleans_backups_only_once_confirmed() {
        let temp = tempfile::tempdir().unwrap();
        let updates = temp.path().join("updates");
        let install_parent = temp.path().join("Programs");
        let backup = install_parent.join(".multicore-previous-0.1.0-9");
        let unrelated = install_parent.join("current");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();

        // First start after an update: the updater still owns the backup.
        super::confirm_started_in(&updates, &install_parent, "0.2.0").unwrap();
        assert_eq!(
            std::fs::read_to_string(updates.join("started-ok")).unwrap(),
            "0.2.0"
        );
        assert!(backup.is_dir());

        // A later start of the same confirmed version removes the stale backup.
        super::confirm_started_in(&updates, &install_parent, "0.2.0").unwrap();
        assert!(!backup.exists());
        assert!(unrelated.is_dir());
    }

    fn mib(value: u64) -> u64 {
        value * 1024 * 1024
    }

    fn downloading(received: u64, total: u64) -> super::UpdateStage {
        super::UpdateStage::Downloading(multicore_release::DownloadProgress { received, total })
    }

    #[test]
    fn an_update_run_moves_through_its_stages_in_order_and_never_back() {
        use super::UpdateStage::{Installing, Restarting, Verifying};
        let mut state = UpdateState::Available(release());
        assert!(!state.advance(Verifying), "nothing runs before the press");
        assert!(state.begin_update().is_some());
        // The size is known from the release before the first byte arrives.
        assert!(matches!(
            state,
            UpdateState::Updating(_, stage) if stage == downloading(0, 1234)
        ));
        assert!(state.advance(downloading(600, 1234)));
        assert!(
            !state.advance(downloading(500, 1234)),
            "a late report never moves the bar back"
        );
        assert!(!state.advance(downloading(600, 1234)));
        assert!(state.advance(Verifying));
        assert!(!state.advance(downloading(1234, 1234)));
        assert!(state.advance(Installing));
        assert!(!state.advance(Verifying));
        assert!(state.advance(Restarting));
        assert!(!state.advance(Installing));
        assert!(matches!(state, UpdateState::Updating(_, Restarting)));
        // A service that did not know the size yet tells it later.
        let mut unknown = UpdateState::Updating(release(), downloading(0, 0));
        assert!(unknown.advance(downloading(0, 1234)));
    }

    #[test]
    fn the_progress_line_shows_real_percent_and_megabytes() {
        let shown =
            UpdateState::Updating(release(), downloading(mib(18), mib(52))).presentation(true);
        assert!(shown.in_progress);
        assert_eq!(shown.toast_title, "Обновление до 0.2.0");
        assert_eq!(shown.toast_copy, "Скачиваем… 34 % · 18 из 52 МБ");
        assert_eq!(shown.status, "Обновление до 0.2.0");
        assert_eq!(shown.detail, shown.toast_copy);
        assert_eq!(shown.progress_permille, 346);
        let done = downloading(mib(52) - 1, mib(52)).text();
        assert_eq!(done, "Скачиваем… 99 % · 52 из 52 МБ");
        // An unknown size draws no fill and claims no figures.
        let unknown = UpdateState::Updating(release(), downloading(mib(3), 0)).presentation(true);
        assert_eq!(unknown.toast_copy, "Скачиваем…");
        assert_eq!(unknown.progress_permille, -1);
        for (stage, text) in [
            (super::UpdateStage::Verifying, "Проверяем…"),
            (super::UpdateStage::Installing, "Устанавливаем…"),
            (super::UpdateStage::Restarting, "Перезапуск…"),
        ] {
            let shown = UpdateState::Updating(release(), stage).presentation(true);
            assert_eq!(shown.toast_copy, text);
            assert_eq!(shown.progress_permille, 1000);
        }
    }

    #[test]
    fn a_second_press_starts_nothing_and_a_failure_offers_a_retry() {
        let mut state = UpdateState::Available(release());
        let offer = state.presentation(true);
        assert!(!offer.in_progress && !offer.failed);
        assert_eq!(offer.toast_title, "Доступно обновление");
        assert!(state.begin_update().is_some());
        let running = state.presentation(true);
        assert!(state.begin_update().is_none(), "already updating");
        assert_ne!(
            offer.toast_key, running.toast_key,
            "the offer's «Позже» does not hide the run"
        );
        state.advance(downloading(1000, 1234));
        assert_eq!(
            state.presentation(true).toast_key,
            running.toast_key,
            "a hidden run stays hidden as it moves"
        );

        state = UpdateState::InstallFailed(release(), "Файл занят".into());
        let failed = state.presentation(true);
        assert!(failed.failed && !failed.in_progress);
        assert_eq!(failed.toast_title, "Не удалось обновить");
        assert_eq!(failed.toast_copy, "Файл занят");
        assert_eq!(failed.action, UpdateAction::Install);
        assert_eq!(failed.action_label, "Повторить");
        assert_ne!(failed.toast_key, running.toast_key);
        assert!(state.begin_update().is_some(), "a retry starts a new run");
    }

    #[test]
    fn the_download_reports_progress_from_a_mock_http_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let body: Vec<u8> = (0..300_000_u32).map(|index| (index % 253) as u8).collect();
        let serve = |declared: usize| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let body = body.clone();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                // In pieces, like a real download.
                for piece in body.chunks(30_000) {
                    if stream.write_all(piece).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
            });
            (format!("http://{address}/asset.zip"), server)
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("asset.zip.partial");

        let (url, server) = serve(body.len());
        let mut seen = Vec::new();
        super::download_to(&client, &url, body.len() as u64, &destination, |progress| {
            seen.push(progress)
        })
        .expect("download");
        server.join().unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), body);
        assert_eq!(seen.first().map(|first| first.received), Some(0));
        assert_eq!(
            seen.last().map(|last| (last.received, last.total)),
            Some((body.len() as u64, body.len() as u64))
        );
        assert!(seen.len() > 2, "progress in between: {seen:?}");
        assert!(
            seen.windows(2)
                .all(|pair| pair[0].received < pair[1].received)
        );

        // The server claims another size than the release: nothing is trusted.
        let (url, server) = serve(body.len() + 1);
        let error =
            super::download_to(&client, &url, body.len() as u64, &destination, |_| {}).unwrap_err();
        let _ = server.join();
        assert_eq!(error, "Размер обновления не совпал с релизом");
    }

    #[test]
    fn the_post_update_confirmation_is_shown_once_and_only_for_this_version() {
        use super::{remember_restart_into_in, take_update_confirmation_in};
        let directory = tempfile::tempdir().unwrap();
        let updates = directory.path().join("updates");
        assert_eq!(take_update_confirmation_in(&updates, "1.2.0"), None);
        remember_restart_into_in(&updates, &version("1.2.0")).unwrap();
        assert_eq!(
            take_update_confirmation_in(&updates, "1.2.0"),
            Some("1.2.0".into())
        );
        assert_eq!(take_update_confirmation_in(&updates, "1.2.0"), None, "once");
        // The update failed and the previous version came back: no claim, and no leftover.
        remember_restart_into_in(&updates, &version("1.2.0")).unwrap();
        assert_eq!(take_update_confirmation_in(&updates, "1.1.10"), None);
        assert_eq!(take_update_confirmation_in(&updates, "1.2.0"), None);
    }

    #[test]
    fn unconfigured_channel_is_explicit_and_has_no_action() {
        let ui = UpdateState::Unconfigured.presentation(true);
        assert_eq!(ui.action, UpdateAction::None);
        assert!(!ui.action_enabled);
        assert!(ui.detail.contains("сборке"));
    }

    #[test]
    fn checking_current_downloading_and_error_states_have_stable_actions() {
        let checking = UpdateState::Checking.presentation(true);
        assert_eq!(checking.action, UpdateAction::None);
        assert!(checking.busy);
        let current = UpdateState::Current.presentation(true);
        assert_eq!(current.action, UpdateAction::Check);
        assert!(current.action_enabled);
        let downloading =
            UpdateState::Updating(release(), super::UpdateStage::Verifying).presentation(true);
        assert_eq!(downloading.action, UpdateAction::None);
        assert!(downloading.busy);
        assert!(
            downloading.action_label.is_empty(),
            "no button while it runs"
        );
        for (configured, action, enabled) in [
            (false, UpdateAction::None, false),
            (true, UpdateAction::Check, true),
        ] {
            let error = UpdateState::Error("network unavailable".into())
                .presentation_with_configuration(true, configured);
            assert_eq!(error.action, action);
            assert_eq!(error.action_enabled, enabled);
            assert!(!error.busy);
            assert_eq!(error.detail, "network unavailable");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::{LinuxInstall, UpdateAction, UpdateState, install_appimage, linux_install_from};
    use multicore_release::GitHubRelease;
    use std::os::unix::fs::PermissionsExt;

    fn release() -> GitHubRelease {
        GitHubRelease::parse_appimage(br#"{
            "tag_name":"v0.9.0",
            "html_url":"https://github.com/fatyzzz/multicore-releases/releases/tag/v0.9.0",
            "assets":[{
                "name":"MultiCore-0.9.0-x86_64.AppImage",
                "browser_download_url":"https://github.com/fatyzzz/multicore-releases/releases/download/v0.9.0/MultiCore-0.9.0-x86_64.AppImage",
                "size":4,
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }"#).unwrap()
    }

    #[test]
    fn install_kind_is_appimage_only_for_an_existing_absolute_image() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("MultiCore.AppImage");
        std::fs::write(&image, b"old").unwrap();
        assert_eq!(
            linux_install_from(Some(image.as_os_str())),
            LinuxInstall::AppImage(image.clone())
        );
        assert_eq!(linux_install_from(None), LinuxInstall::Package);
        assert_eq!(
            linux_install_from(Some("MultiCore.AppImage".as_ref())),
            LinuxInstall::Package
        );
        assert_eq!(
            linux_install_from(Some(temp.path().join("missing").as_os_str())),
            LinuxInstall::Package
        );
    }

    #[test]
    fn package_managed_updates_are_announced_without_an_install_action() {
        let ui = UpdateState::Managed(release()).presentation(true);
        assert!(ui.available);
        assert_eq!(ui.action, UpdateAction::None);
        assert!(!ui.action_enabled);
        assert!(ui.status.contains("0.9.0"));
        assert!(ui.detail.contains("менеджер пакетов"));
        let appimage = UpdateState::Available(release()).presentation(true);
        assert_eq!(appimage.action, UpdateAction::Install);
        assert!(appimage.detail.contains("AppImage"));
    }

    #[test]
    fn appimage_is_replaced_atomically_and_the_previous_image_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("MultiCore.AppImage");
        let staged = temp.path().join(".MultiCore.AppImage.update");
        let backup = temp.path().join("MultiCore.AppImage.old");
        std::fs::write(&image, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        std::fs::write(&backup, b"older").unwrap();

        install_appimage(&image, &staged, &backup).unwrap();
        assert_eq!(std::fs::read(&image).unwrap(), b"new");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old");
        assert!(!staged.exists());
        let mode = std::fs::metadata(&image).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

#[cfg(test)]
mod english_tests {
    use super::*;
    use multicore_i18n::{Lang, ThreadLanguage};

    fn cyrillic(text: &str) -> bool {
        text.chars().any(|c| ('\u{400}'..='\u{4ff}').contains(&c))
    }

    fn release() -> GitHubRelease {
        GitHubRelease::parse_for_arch(
            br#"{
            "tag_name":"v0.2.0",
            "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
            "assets":[{
                "name":"multicore-windows-x64.zip",
                "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
                "size":1234,
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }"#,
            multicore_release::WindowsArch::X64,
        )
        .unwrap()
    }

    #[test]
    fn every_update_state_reads_in_english() {
        let _en = ThreadLanguage::set(Lang::En);
        let states = [
            UpdateState::Unconfigured,
            UpdateState::Checking,
            UpdateState::Deferred,
            UpdateState::Current,
            UpdateState::Available(release()),
            UpdateState::Managed(release()),
            UpdateState::Updating(
                release(),
                UpdateStage::Downloading(multicore_release::DownloadProgress {
                    received: 18 << 20,
                    total: 52 << 20,
                }),
            ),
            UpdateState::Updating(
                release(),
                UpdateStage::Downloading(multicore_release::DownloadProgress::default()),
            ),
            UpdateState::Updating(release(), UpdateStage::Verifying),
            UpdateState::Updating(release(), UpdateStage::Installing),
            UpdateState::Updating(release(), UpdateStage::Restarting),
            UpdateState::InstallFailed(release(), "swap failed".into()),
            UpdateState::Error("GitHub is unavailable".into()),
        ];
        for state in states {
            for idle in [true, false] {
                for configured in [true, false] {
                    let shown = state.presentation_with_configuration(idle, configured);
                    for text in [
                        &shown.status,
                        &shown.detail,
                        &shown.action_label,
                        &shown.toast_title,
                        &shown.toast_copy,
                    ] {
                        assert!(!cyrillic(text), "{state:?}: {text}");
                    }
                }
            }
        }
        let available = UpdateState::Available(release()).presentation(true);
        assert_eq!(available.status, "Update 0.2.0 available");
        assert_eq!(available.action_label, "Install");
        let running = UpdateState::Updating(
            release(),
            UpdateStage::Downloading(multicore_release::DownloadProgress {
                received: 18 << 20,
                total: 52 << 20,
            }),
        )
        .presentation(true);
        assert_eq!(running.toast_title, "Updating to 0.2.0");
        assert_eq!(running.toast_copy, "Downloading… 34% · 18 of 52 MB");
    }
}
