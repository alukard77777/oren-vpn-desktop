use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use multicore_release::{GitHubRelease, ReleaseDecision, Repository, Version, verify_archive_file};

const RELEASE_BODY_LIMIT: usize = 512 * 1024;
const UPDATE_USER_AGENT: &str = concat!("multicore-updater/", env!("CARGO_PKG_VERSION"));
#[cfg(target_os = "linux")]
const REPLACEMENT_DETAIL: &str = "Будет заменён файл AppImage";
#[cfg(not(target_os = "linux"))]
const REPLACEMENT_DETAIL: &str = "Будет заменён весь portable-пакет";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateAction {
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
    Downloading(GitHubRelease),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdatePresentation {
    pub status: String,
    pub detail: String,
    pub action_label: String,
    pub action: UpdateAction,
    pub action_enabled: bool,
    pub available: bool,
    pub busy: bool,
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
        match self {
            Self::Unconfigured => UpdatePresentation {
                status: format!("MultiCore {}", env!("CARGO_PKG_VERSION")),
                detail: "Канал обновлений задаётся при сборке".into(),
                action_label: String::new(),
                action: UpdateAction::None,
                action_enabled: false,
                available: false,
                busy: false,
            },
            Self::Checking => UpdatePresentation {
                status: "Проверяем обновления…".into(),
                detail: format!("Установлена версия {}", env!("CARGO_PKG_VERSION")),
                action_label: "Проверяем…".into(),
                action: UpdateAction::None,
                action_enabled: false,
                available: false,
                busy: true,
            },
            Self::Current => UpdatePresentation {
                status: "Установлена актуальная версия".into(),
                detail: format!("MultiCore {}", env!("CARGO_PKG_VERSION")),
                action_label: "Проверить".into(),
                action: UpdateAction::Check,
                action_enabled: true,
                available: false,
                busy: false,
            },
            Self::Available(release) => UpdatePresentation {
                status: format!("Доступна версия {}", release.version()),
                detail: if runtime_idle {
                    REPLACEMENT_DETAIL.into()
                } else {
                    "Туннель будет остановлен, а клиент перезапущен".into()
                },
                action_label: "Обновить".into(),
                action: UpdateAction::Install,
                action_enabled: true,
                available: true,
                busy: false,
            },
            Self::Managed(release) => UpdatePresentation {
                status: format!("Доступна версия {}", release.version()),
                detail: "Обновите пакет через менеджер пакетов".into(),
                action_label: String::new(),
                action: UpdateAction::None,
                action_enabled: false,
                available: true,
                busy: false,
            },
            Self::Downloading(release) => UpdatePresentation {
                status: format!("Загружаем версию {}…", release.version()),
                detail: "Проверяем размер и SHA-256 перед установкой".into(),
                action_label: "Загружаем…".into(),
                action: UpdateAction::None,
                action_enabled: false,
                available: true,
                busy: true,
            },
            Self::Error(message) => UpdatePresentation {
                status: "Не удалось проверить обновления".into(),
                detail: message.clone(),
                action_label: if configured {
                    "Повторить".into()
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
            },
        }
    }

    pub(crate) fn available_release(&self) -> Option<GitHubRelease> {
        match self {
            Self::Available(release) => Some(release.clone()),
            _ => None,
        }
    }
}

pub(crate) fn configured_repository() -> Result<Option<Repository>, String> {
    let Some(value) = option_env!("MULTICORE_UPDATE_REPOSITORY") else {
        return Ok(None);
    };
    Repository::parse(value)
        .map(Some)
        .map_err(|_| "В сборке указан неверный GitHub-репозиторий".into())
}

pub(crate) fn check_latest() -> Result<UpdateState, String> {
    let repository =
        configured_repository()?.ok_or_else(|| "Канал обновлений не настроен".to_owned())?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::limited(2))
        .build()
        .map_err(|_| "Не удалось создать защищённое соединение".to_owned())?;
    let mut response = client
        .get(repository.latest_release_api_url())
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| "GitHub сейчас недоступен".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "GitHub ответил HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > RELEASE_BODY_LIMIT as u64)
    {
        return Err("Ответ GitHub слишком большой".into());
    }
    let mut body = Vec::new();
    response
        .by_ref()
        .take(RELEASE_BODY_LIMIT as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "Не удалось прочитать ответ GitHub".to_owned())?;
    if body.len() > RELEASE_BODY_LIMIT {
        return Err("Ответ GitHub слишком большой".into());
    }
    #[cfg(not(target_os = "linux"))]
    let release = GitHubRelease::parse_for_repository(&repository, &body);
    #[cfg(target_os = "linux")]
    let release = GitHubRelease::parse_appimage_for_repository(&repository, &body);
    let release = release.map_err(|_| "Релиз не соответствует контракту MultiCore".to_owned())?;
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|_| "Версия клиента собрана неверно".to_owned())?;
    Ok(match release.decision(&current) {
        ReleaseDecision::Current => UpdateState::Current,
        ReleaseDecision::UpdateAvailable => available_state(release),
    })
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

#[cfg(target_os = "linux")]
pub(crate) fn download_and_start_apply(release: &GitHubRelease) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let LinuxInstall::AppImage(image) = linux_install() else {
        return Err("Обновите MultiCore через менеджер пакетов".into());
    };
    let directory = image
        .parent()
        .ok_or_else(|| "Некорректный путь AppImage".to_owned())?;
    let name = image
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Некорректное имя AppImage".to_owned())?;
    let staged = directory.join(format!(".{name}.update"));
    let backup = directory.join(format!("{name}.old"));
    if staged.exists() {
        fs::remove_file(&staged).map_err(|_| "Не удалось очистить прошлую загрузку".to_owned())?;
    }
    download_release(release, &staged)?;
    if verify_archive_file(
        &staged,
        release.asset().size(),
        release.asset().sha256_hex(),
    )
    .is_err()
    {
        let _ = fs::remove_file(&staged);
        return Err("Загруженный AppImage не прошёл SHA-256 проверку".into());
    }
    install_appimage(&image, &staged, &backup)
        .map_err(|_| "Не удалось заменить файл AppImage".to_owned())?;
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
        .map_err(|_| "Не удалось перезапустить MultiCore".to_owned())?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn download_and_start_apply(release: &GitHubRelease) -> Result<(), String> {
    let update_dir = update_directory()?;
    fs::create_dir_all(&update_dir)
        .map_err(|_| "Не удалось создать папку обновлений".to_owned())?;
    let partial = update_dir.join(format!("multicore-{}.zip.partial", release.version()));
    let archive = update_dir.join(format!("multicore-{}.zip", release.version()));
    for stale in [&partial, &archive] {
        if stale.exists() {
            fs::remove_file(stale)
                .map_err(|_| "Не удалось очистить прошлую загрузку".to_owned())?;
        }
    }

    download_release(release, &partial)?;
    verify_archive_file(
        &partial,
        release.asset().size(),
        release.asset().sha256_hex(),
    )
    .map_err(|_| "Загруженный архив не прошёл SHA-256 проверку".to_owned())?;
    fs::rename(&partial, &archive)
        .map_err(|_| "Не удалось зафиксировать загруженный архив".to_owned())?;

    let current_exe =
        std::env::current_exe().map_err(|_| "Не удалось определить путь MultiCore".to_owned())?;
    let target = current_exe
        .parent()
        .ok_or_else(|| "Некорректный путь MultiCore".to_owned())?
        .to_path_buf();
    let helper_source = target.join("runtime").join("multicore-updater.exe");
    if !helper_source.is_file() {
        return Err("В portable-пакете нет runtime/multicore-updater.exe".into());
    }
    let helper = update_dir.join(format!("multicore-apply-{}.exe", release.version()));
    fs::copy(&helper_source, &helper)
        .map_err(|_| "Не удалось подготовить helper обновления".to_owned())?;

    let mut command = Command::new(helper);
    command
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .arg("--archive")
        .arg(&archive)
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
        .map_err(|_| "Не удалось запустить helper обновления".to_owned())?;
    Ok(())
}

fn download_release(release: &GitHubRelease, destination: &Path) -> Result<(), String> {
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
        .map_err(|_| "Не удалось создать соединение для загрузки".to_owned())?;
    let mut response = client
        .get(release.asset().download_url())
        .header("User-Agent", UPDATE_USER_AGENT)
        .send()
        .map_err(|_| "Не удалось загрузить обновление".to_owned())?;
    if !response.status().is_success() {
        return Err(format!(
            "Загрузка обновления вернула HTTP {}",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != release.asset().size())
    {
        return Err("Размер обновления не совпал с релизом".into());
    }
    let mut output =
        File::create(destination).map_err(|_| "Не удалось создать временный архив".to_owned())?;
    let mut remaining = release.asset().size();
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let limit = buffer.len().min(remaining as usize);
        let read = response
            .read(&mut buffer[..limit])
            .map_err(|_| "Загрузка обновления прервалась".to_owned())?;
        if read == 0 {
            return Err("Обновление загрузилось не полностью".into());
        }
        output
            .write_all(&buffer[..read])
            .map_err(|_| "Не удалось записать обновление".to_owned())?;
        remaining -= read as u64;
    }
    let mut extra = [0_u8; 1];
    if response
        .read(&mut extra)
        .map_err(|_| "Ошибка завершения загрузки".to_owned())?
        != 0
    {
        return Err("Обновление больше заявленного размера".into());
    }
    output
        .flush()
        .map_err(|_| "Не удалось сохранить обновление".to_owned())?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn update_directory() -> Result<PathBuf, String> {
    multicore_platform::dirs::user_data_root()
        .map(|root| root.join("updates"))
        .map_err(|_| "Каталог данных MultiCore недоступен".to_owned())
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
    use multicore_release::GitHubRelease;

    fn release() -> GitHubRelease {
        GitHubRelease::parse(br#"{
            "tag_name":"v0.2.0",
            "html_url":"https://github.com/Fatyzzz/multi-core/releases/tag/v0.2.0",
            "assets":[{
                "name":"multicore-windows-x64.zip",
                "browser_download_url":"https://github.com/Fatyzzz/multi-core/releases/download/v0.2.0/multicore-windows-x64.zip",
                "size":1234,
                "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            }]
        }"#).unwrap()
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
        let downloading = UpdateState::Downloading(release()).presentation(true);
        assert_eq!(downloading.action, UpdateAction::None);
        assert!(downloading.busy);
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
