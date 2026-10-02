//! Settings card for per-user installs: download the release installer and run it in service
//! mode (`/ALLUSERS`, one UAC). The installer removes this per-user copy and keeps the user's
//! data, so MultiCore quits first and the installer starts only after the app has let go of
//! its files (see `launch_pending_installer`, called by `main` after the event loop ends).

use multicore_i18n::tr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The verified installer to start once the app has fully shut down.
static PENDING_INSTALLER: Mutex<Option<PathBuf>> = Mutex::new(None);

pub(crate) fn is_per_user_install(executable: &Path, local_app_data: &Path) -> bool {
    let normalize = |path: &Path| path.to_string_lossy().replace('/', "\\").to_lowercase();
    normalize(executable).starts_with(&(normalize(local_app_data) + "\\programs\\multicore\\"))
}

fn card_visible_on(windows: bool, per_user_install: bool, service_mode: bool) -> bool {
    windows && per_user_install && !service_mode
}

/// The card shows on Windows for a per-user install while no service drives the daemon.
/// `service_mode` is true when the daemon reports it runs on the service or the service is
/// registered at all. Called on every snapshot, so the install location is read once.
pub(crate) fn card_visible(service_mode: bool) -> bool {
    card_visible_on(cfg!(windows), per_user_install(), service_mode)
}

/// This copy runs from `%LOCALAPPDATA%\Programs\MultiCore` (read once).
pub(crate) fn per_user_install() -> bool {
    static PER_USER_INSTALL: OnceLock<bool> = OnceLock::new();
    *PER_USER_INSTALL.get_or_init(|| {
        match (std::env::current_exe(), std::env::var_os("LOCALAPPDATA")) {
            (Ok(executable), Some(local)) => is_per_user_install(&executable, Path::new(&local)),
            _ => false,
        }
    })
}

/// Whether the MultiCore service is registered, asked of the service manager once per run:
/// installing it restarts the app anyway.
pub(crate) fn service_installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(query_service_installed)
}

/// An unreadable service manager counts as "present", so the card never offers an install
/// that cannot be checked.
#[cfg(windows)]
fn query_service_installed() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, GetLastError};
    use windows_sys::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
    };

    let name: Vec<u16> = "MultiCore".encode_utf16().chain([0]).collect();
    unsafe {
        let manager = OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT);
        if manager.is_null() {
            return true;
        }
        let service = OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS);
        let installed = if service.is_null() {
            GetLastError() != ERROR_SERVICE_DOES_NOT_EXIST
        } else {
            CloseServiceHandle(service);
            true
        };
        CloseServiceHandle(manager);
        installed
    }
}

#[cfg(not(windows))]
fn query_service_installed() -> bool {
    false
}

/// Inno Setup command line: service mode without the privileges dialog, a progress window
/// only, and a relaunch of MultiCore once the service runs. Never the data-cleanup task.
fn installer_arguments(launch_at_sign_in: bool) -> Vec<&'static str> {
    let mut arguments = vec![
        "/ALLUSERS",
        "/SILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/LAUNCH=1",
    ];
    if launch_at_sign_in {
        arguments.push("/MERGETASKS=autostart");
    }
    arguments
}

/// Download the installer of the latest release from the pinned repository and verify it.
#[cfg(windows)]
pub(crate) fn download_installer() -> Result<PathBuf, String> {
    use multicore_release::{GitHubRelease, verify_archive_file};

    let repository = crate::updater::configured_repository()?
        .ok_or_else(|| tr!("Канал обновлений не настроен").to_owned())?;
    let body = crate::updater::fetch_latest_release_body(&repository)?;
    let release = GitHubRelease::parse_installer_for_repository(&repository, &body)
        .map_err(|_| tr!("В релизе нет установщика со службой").to_owned())?;
    let directory = crate::updater::update_directory()?;
    std::fs::create_dir_all(&directory)
        .map_err(|_| tr!("Не удалось создать папку обновлений").to_owned())?;
    let installer = directory.join(format!("MultiCore-Setup-{}.exe", release.version()));
    if installer.exists() {
        std::fs::remove_file(&installer)
            .map_err(|_| tr!("Не удалось очистить прошлую загрузку").to_owned())?;
    }
    crate::updater::download_release_to(&release, &installer)?;
    verify_archive_file(
        &installer,
        release.asset().size(),
        release.asset().sha256_hex(),
    )
    .map_err(|_| tr!("Установщик не прошёл SHA-256 проверку").to_owned())?;
    Ok(installer)
}

#[cfg(not(windows))]
pub(crate) fn download_installer() -> Result<PathBuf, String> {
    Err(tr!("Служба устанавливается пакетом").into())
}

/// Remember the verified installer; `main` starts it after MultiCore has shut down.
pub(crate) fn launch_after_exit(installer: PathBuf) {
    *PENDING_INSTALLER.lock().expect("pending installer lock") = Some(installer);
}

/// Start the remembered installer. Called once the window, tray, daemon and single-instance
/// guard are gone, so the installer's migration step can remove the per-user copy.
pub(crate) fn launch_pending_installer() {
    let Some(installer) = PENDING_INSTALLER
        .lock()
        .expect("pending installer lock")
        .take()
    else {
        return;
    };
    let launch_at_sign_in = matches!(
        crate::windows_settings::query_launch_at_sign_in(),
        Ok(crate::windows_settings::LaunchAtSignInState::Enabled)
    );
    // Inno Setup elevates itself (one UAC) and relaunches MultiCore (/LAUNCH=1).
    if let Err(error) = std::process::Command::new(&installer)
        .args(installer_arguments(launch_at_sign_in))
        .spawn()
    {
        eprintln!("service installer could not start: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::{card_visible_on, installer_arguments, is_per_user_install};
    use std::path::Path;

    #[test]
    fn only_a_per_user_install_offers_the_service() {
        let local = Path::new(r"C:\Users\me\AppData\Local");
        assert!(is_per_user_install(
            Path::new(r"C:\Users\me\AppData\Local\Programs\MultiCore\current\MultiCore.exe"),
            local
        ));
        assert!(is_per_user_install(
            Path::new(r"c:\users\ME\appdata\local\programs\multicore\current\MultiCore.exe"),
            local
        ));
        assert!(!is_per_user_install(
            Path::new(r"C:\Program Files\MultiCore\current\MultiCore.exe"),
            local
        ));
        assert!(!is_per_user_install(
            Path::new(r"D:\portable\MultiCore\MultiCore.exe"),
            local
        ));
    }

    #[test]
    fn the_card_needs_windows_a_per_user_install_and_no_service() {
        assert!(card_visible_on(true, true, false));
        assert!(!card_visible_on(true, true, true));
        assert!(!card_visible_on(true, false, false));
        assert!(!card_visible_on(false, true, false));
    }

    #[test]
    fn every_snapshot_hides_the_card_while_the_daemon_runs_on_the_service() {
        let main = include_str!("main.rs");
        let apply: String = main
            .split("fn apply_snapshot(ui: &AppWindow")
            .nth(1)
            .expect("apply_snapshot source")
            .split_whitespace()
            .collect();
        assert!(apply.contains(
            "service_install::card_visible(snapshot.service_mode||service_install::service_installed(),"
        ));
        assert!(main.contains("service_mode: model.service_mode(),"));
    }

    #[test]
    fn the_installer_runs_in_service_mode_silently_and_relaunches_the_app() {
        let arguments = installer_arguments(false);
        assert_eq!(
            arguments,
            [
                "/ALLUSERS",
                "/SILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/LAUNCH=1"
            ]
        );
        // Launch at sign-in survives the removal of the per-user copy.
        let with_autostart = installer_arguments(true);
        assert_eq!(
            with_autostart.last().copied(),
            Some("/MERGETASKS=autostart")
        );
        // Never the "delete old subscriptions and settings" task.
        for argument in arguments.iter().chain(&with_autostart) {
            assert!(!argument.contains("cleandata"), "{argument}");
        }
    }

    #[test]
    fn the_service_offer_toast_follows_the_card_and_is_declined_for_good() {
        let ui: String = include_str!("../ui/app.slint").split_whitespace().collect();
        assert!(ui.contains(
            "service-toast-visible:root.service-card-visible&&root.has-profile&&!root.service-offer-dismissed;"
        ));
        assert!(ui.contains("\"Мгновенноеподключение\""));
        assert!(ui.contains("\"Установитеслужбу—VPNбудетвключатьсябеззапросаправ\""));
        // «Установить» is the Settings card's own flow; «Не сейчас» is remembered.
        assert!(ui.contains("label:root.service-install-busy?\"Загружаем…\":\"Установить\";"));
        assert!(ui.contains("activated=>{root.install-service();}"));
        assert!(ui.contains("label:\"Несейчас\";"));
        assert!(ui.contains("activated=>{root.service-offer-dismissed=true;}"));
        let main: String = include_str!("main.rs").split_whitespace().collect();
        assert!(main.contains(
            "ui.set_service_offer_dismissed(loaded_preferences.service_offer_dismissed);"
        ));
        assert_eq!(
            main.matches("tracker.set_service_offer_dismissed(ui.get_service_offer_dismissed());")
                .count(),
            2,
            "saved by the periodic writer and the final flush"
        );
    }
}
