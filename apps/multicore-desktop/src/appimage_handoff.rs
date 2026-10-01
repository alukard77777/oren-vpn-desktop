//! The AppImage and a system package (deb/rpm/pacman) installed side by side.
//!
//! Both are the same app for the same user: one single-instance lock, one data directory, one
//! autostart entry. The package additionally carries the always-ready system service, which
//! accepts only the installed daemon image, while the AppImage would fall back to on-demand
//! `pkexec` elevation, update itself on its own schedule and possibly run another version
//! against the same data.
//!
//! So an AppImage started while the package is installed hands off: it executes the installed
//! app with the same arguments (a `multicore://` deep link or `--background` from autostart
//! included) and never starts its own daemon. The installed app then either becomes the
//! primary instance or forwards the activation to the one already running. Whatever the
//! desktop picked as the URL handler or autostart entry, everything ends in the installed app.
//!
//! `MULTICORE_APPIMAGE_STANDALONE=1` keeps the AppImage running on its own (it then works in
//! on-demand mode: the service refuses its daemon and it falls back cleanly).
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The installed app of the deb/rpm/pacman packages (`/usr/bin/multicore` links to it).
pub(crate) const INSTALLED_APP: &str = "/usr/lib/multicore/multicore";
/// The installed daemon: the only image the system service accepts.
pub(crate) const INSTALLED_DAEMON: &str = "/usr/lib/multicore/runtime/multicore-daemon";
/// Set to `1` to keep the AppImage running next to an installed package.
pub(crate) const STANDALONE_ENV: &str = "MULTICORE_APPIMAGE_STANDALONE";
/// Set for the installed app by a hand-off: the AppImage file it was started from.
pub(crate) const HANDED_OFF_FROM_ENV: &str = "MULTICORE_HANDED_OFF_FROM";
/// The AppImage runtime's variables. They must not reach the installed app, or it would
/// believe it runs from the AppImage (self-update, autostart and relaunch targets).
pub(crate) const APPIMAGE_RUNTIME_ENV: [&str; 4] = ["APPIMAGE", "APPDIR", "ARGV0", "OWD"];

/// What the process environment says about this launch.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Launch<'a> {
    /// `$APPIMAGE`: the AppImage file this process runs from.
    pub(crate) appimage: Option<&'a OsStr>,
    /// `$MULTICORE_APPIMAGE_STANDALONE`.
    pub(crate) standalone: Option<&'a OsStr>,
    /// `$MULTICORE_HANDED_OFF_FROM`: this process is already the target of a hand-off.
    pub(crate) handed_off_from: Option<&'a OsStr>,
}

/// The AppImage file this launch runs from, when it is one.
pub(crate) fn appimage_file<'a>(launch: &Launch<'a>) -> Option<&'a Path> {
    launch
        .appimage
        .filter(|value| !value.is_empty())
        .map(Path::new)
        .filter(|path| path.is_absolute())
}

/// The installed app to hand off to, or `None` to run this process as usual: not an AppImage,
/// the standalone override, already handed off (never a loop), or no complete package
/// under `root` (`/` in production).
pub(crate) fn handoff_target(
    launch: &Launch<'_>,
    root: &Path,
    is_executable: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    appimage_file(launch)?;
    if launch.standalone.is_some_and(|value| value == "1") || launch.handed_off_from.is_some() {
        return None;
    }
    let app = under(root, INSTALLED_APP);
    (is_executable(&app) && is_executable(&under(root, INSTALLED_DAEMON))).then_some(app)
}

fn under(root: &Path, absolute: &str) -> PathBuf {
    root.join(absolute.trim_start_matches('/'))
}

/// An AppImage started while the system package is installed becomes the installed app; this
/// returns only when there is nothing to hand off to or the hand-off failed (the AppImage then
/// runs on its own, as before).
#[cfg(target_os = "linux")]
pub(crate) fn hand_off_to_installed_package() {
    use std::os::unix::process::CommandExt;

    let appimage = std::env::var_os("APPIMAGE");
    let standalone = std::env::var_os(STANDALONE_ENV);
    let handed_off_from = std::env::var_os(HANDED_OFF_FROM_ENV);
    let launch = Launch {
        appimage: appimage.as_deref(),
        standalone: standalone.as_deref(),
        handed_off_from: handed_off_from.as_deref(),
    };
    let Some(app) = handoff_target(&launch, Path::new("/"), is_executable_file) else {
        return;
    };
    let Some(image) = appimage_file(&launch) else {
        return;
    };
    let mut command = std::process::Command::new(&app);
    command
        .args(std::env::args_os().skip(1))
        .env(HANDED_OFF_FROM_ENV, image);
    for name in APPIMAGE_RUNTIME_ENV {
        command.env_remove(name);
    }
    eprintln!(
        "MultiCore is installed as a system package: starting {} instead of this AppImage \
         (set {STANDALONE_ENV}=1 to keep the AppImage)",
        app.display()
    );
    // Only returns on failure.
    let error = command.exec();
    eprintln!("hand-off to the installed MultiCore failed: {error}");
}

#[cfg(target_os = "linux")]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// The installed app was started by an AppImage hand-off: the AppImage file, if so.
#[cfg(target_os = "linux")]
pub(crate) fn handed_off_from() -> Option<PathBuf> {
    if std::env::var_os("APPIMAGE").is_some() {
        return None;
    }
    std::env::var_os(HANDED_OFF_FROM_ENV)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::{INSTALLED_APP, INSTALLED_DAEMON, Launch, appimage_file, handoff_target};
    use std::collections::HashSet;
    use std::ffi::OsStr;
    use std::path::{Path, PathBuf};

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"C:\root"
        } else {
            "/root-under-test"
        })
    }

    fn installed(root: &Path, entries: &[&str]) -> HashSet<PathBuf> {
        entries
            .iter()
            .map(|entry| root.join(entry.trim_start_matches('/')))
            .collect()
    }

    fn image() -> &'static OsStr {
        OsStr::new(if cfg!(windows) {
            r"C:\home\me\MultiCore-1.1.1-x86_64.AppImage"
        } else {
            "/home/me/Apps/MultiCore-1.1.1-x86_64.AppImage"
        })
    }

    #[test]
    fn an_appimage_hands_off_to_a_complete_installed_package() {
        let root = root();
        let files = installed(&root, &[INSTALLED_APP, INSTALLED_DAEMON]);
        let launch = Launch {
            appimage: Some(image()),
            ..Launch::default()
        };
        assert_eq!(
            handoff_target(&launch, &root, |path| files.contains(path)),
            Some(root.join("usr/lib/multicore/multicore"))
        );
    }

    #[test]
    fn nothing_is_handed_off_without_a_complete_package() {
        let root = root();
        let launch = Launch {
            appimage: Some(image()),
            ..Launch::default()
        };
        for entries in [&[][..], &[INSTALLED_APP][..], &[INSTALLED_DAEMON][..]] {
            let files = installed(&root, entries);
            assert_eq!(
                handoff_target(&launch, &root, |path| files.contains(path)),
                None,
                "{entries:?}"
            );
        }
    }

    #[test]
    fn the_package_itself_the_override_and_a_second_hop_never_hand_off() {
        let root = root();
        let files = installed(&root, &[INSTALLED_APP, INSTALLED_DAEMON]);
        let exists = |path: &Path| files.contains(path);
        // The installed app (no AppImage runtime) and a bogus relative $APPIMAGE.
        assert_eq!(handoff_target(&Launch::default(), &root, exists), None);
        for appimage in ["", "MultiCore.AppImage"] {
            let launch = Launch {
                appimage: Some(OsStr::new(appimage)),
                ..Launch::default()
            };
            assert_eq!(handoff_target(&launch, &root, exists), None, "{appimage:?}");
        }
        let standalone = Launch {
            appimage: Some(image()),
            standalone: Some(OsStr::new("1")),
            ..Launch::default()
        };
        assert_eq!(handoff_target(&standalone, &root, exists), None);
        let not_standalone = Launch {
            standalone: Some(OsStr::new("0")),
            ..standalone
        };
        assert!(handoff_target(&not_standalone, &root, exists).is_some());
        let second_hop = Launch {
            appimage: Some(image()),
            handed_off_from: Some(image()),
            ..Launch::default()
        };
        assert_eq!(handoff_target(&second_hop, &root, exists), None);
    }

    #[test]
    fn the_appimage_file_is_only_an_absolute_path() {
        let launch = Launch {
            appimage: Some(image()),
            ..Launch::default()
        };
        assert_eq!(appimage_file(&launch), Some(Path::new(image())));
        assert_eq!(appimage_file(&Launch::default()), None);
    }

    #[test]
    fn the_hand_off_runs_before_anything_is_claimed_and_the_target_adopts_autostart() {
        let main = include_str!("main.rs");
        let production = main.split("mod window_tests {").next().unwrap();
        let compact: String = production.split_whitespace().collect();
        let hand_off = compact
            .find("appimage_handoff::hand_off_to_installed_package();")
            .expect("main hands off");
        let claim = compact
            .find("single_instance::claim_or_forward(")
            .expect("single-instance claim");
        let bootstrap = compact.find("bootstrap::start()").expect("daemon start");
        assert!(hand_off < claim && hand_off < bootstrap);
        let adopt = compact
            .find("windows_settings::adopt_launch_at_sign_in_from(")
            .expect("autostart adoption");
        assert!(claim < adopt, "only the primary instance touches autostart");
    }
}
