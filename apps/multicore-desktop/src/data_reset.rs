//! "Удалить все данные": the app relaunches itself with `--reset-data` and quits. The new
//! process waits until the old instance has released the single-instance guard (by then its
//! owned daemon has been stopped and reaped), deletes the MultiCore data directory while holding
//! the guard, then starts the app normally with a clean profile.
//!
//! The data directory is `%LOCALAPPDATA%\MultiCore` on Windows and `~/.local/share/multicore`
//! (or `$XDG_DATA_HOME/multicore`) on Linux: subscriptions, preferences, staged runtimes and
//! logs. The always-ready service's own directory is root/SYSTEM-only and is removed by the
//! package manager or the uninstaller instead.

use std::{
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use crate::single_instance::{self, Activation, Claim};

pub(crate) const RESET_FLAG: &str = "--reset-data";
const RESET_ATTEMPTS: u32 = 60;
const RESET_DELAY: Duration = Duration::from_millis(250);
/// How long the resetting process waits for the old instance to exit.
const TAKEOVER_TIMEOUT: Duration = Duration::from_secs(60);
const TAKEOVER_POLL: Duration = Duration::from_millis(250);

/// Only a directory literally named `MultiCore`/`multicore` may be deleted, so a broken
/// `LOCALAPPDATA`/`XDG_DATA_HOME` can never turn this into "delete my home".
pub(crate) fn is_multicore_data_root(path: &Path) -> bool {
    path.parent().is_some()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("multicore"))
}

/// Calls `remove` until it succeeds or reports the directory gone; other failures (a file still
/// held by the exiting daemon on Windows) are retried `attempts` times, `delay` apart.
pub(crate) fn remove_with_retry(
    root: &Path,
    attempts: u32,
    delay: Duration,
    mut remove: impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            std::thread::sleep(delay);
        }
        match remove(root) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("data directory was not removed")))
}

/// The program to start again: the AppImage file itself when running from one (its mount
/// disappears with this process), else this executable.
fn launcher() -> io::Result<PathBuf> {
    launcher_from(
        std::env::var_os("APPIMAGE").as_deref(),
        std::env::current_exe()?,
    )
}

fn launcher_from(appimage: Option<&OsStr>, current_exe: PathBuf) -> io::Result<PathBuf> {
    Ok(match appimage.map(Path::new) {
        Some(appimage) if cfg!(target_os = "linux") && appimage.is_absolute() => {
            appimage.to_path_buf()
        }
        _ => current_exe,
    })
}

/// Starts the resetting process. The caller quits right after, which releases the guard.
pub(crate) fn schedule_reset_and_relaunch() -> io::Result<()> {
    Command::new(launcher()?).arg(RESET_FLAG).spawn().map(drop)
}

/// Handles `--reset-data` before anything else runs. Returns true when it was requested; the
/// caller then exits without starting the UI (this function already started the clean app).
pub(crate) fn run_pending_reset(arguments: &[OsString]) -> bool {
    if !arguments.iter().any(|argument| argument == RESET_FLAG) {
        return false;
    }
    let guard = wait_for_the_old_instance();
    match multicore_platform::dirs::user_data_root() {
        Ok(root) if is_multicore_data_root(&root) => {
            if let Err(error) = remove_with_retry(&root, RESET_ATTEMPTS, RESET_DELAY, |path| {
                std::fs::remove_dir_all(path)
            }) {
                eprintln!("MultiCore data was not fully removed: {error}");
            }
        }
        Ok(_) => eprintln!("MultiCore data directory has an unexpected name; nothing removed"),
        Err(error) => eprintln!("MultiCore data directory unavailable: {error}"),
    }
    drop(guard);
    // Start the real app without the flag, so launch-argument parsing never sees it.
    if let Err(error) = launcher().and_then(|program| Command::new(program).spawn()) {
        eprintln!("MultiCore could not be started again: {error}");
    }
    true
}

/// Holds the single-instance guard once the old instance has let go of it. `Background` is the
/// only activation a still-running old instance ignores. `None` after the timeout: the removal
/// then still runs, and the retry loop copes with files the old process keeps open.
fn wait_for_the_old_instance() -> Option<Claim> {
    let deadline = Instant::now() + TAKEOVER_TIMEOUT;
    loop {
        match single_instance::claim_or_forward(&Activation::Background) {
            Ok(claim @ Claim::Primary { .. }) => return Some(claim),
            Ok(Claim::Forwarded) | Err(_) if Instant::now() < deadline => {
                std::thread::sleep(TAKEOVER_POLL);
            }
            Ok(Claim::Forwarded) | Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RESET_FLAG, is_multicore_data_root, launcher_from, remove_with_retry};
    use std::{cell::Cell, ffi::OsStr, io, path::Path, path::PathBuf, time::Duration};

    #[test]
    fn only_multicore_data_directories_may_be_deleted() {
        // Backslash is a path separator only on Windows.
        assert_eq!(
            is_multicore_data_root(Path::new(r"C:\Users\me\AppData\Local\MultiCore")),
            cfg!(windows)
        );
        assert!(is_multicore_data_root(Path::new(
            "/home/me/.local/share/multicore"
        )));
        assert!(!is_multicore_data_root(Path::new("/home/me")));
        assert!(!is_multicore_data_root(Path::new(
            r"C:\Users\me\AppData\Local"
        )));
        assert!(!is_multicore_data_root(Path::new("/")));
        assert!(!is_multicore_data_root(Path::new("multicore/..")));
    }

    #[test]
    fn removal_retries_while_the_old_daemon_holds_files() {
        let calls = Cell::new(0);
        remove_with_retry(Path::new("/x/multicore"), 5, Duration::ZERO, |_| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(calls.get(), 3);
        remove_with_retry(Path::new("/x/multicore"), 5, Duration::ZERO, |_| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        })
        .unwrap();
        let calls = Cell::new(0);
        assert!(
            remove_with_retry(Path::new("/x/multicore"), 2, Duration::ZERO, |_| {
                calls.set(calls.get() + 1);
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            })
            .is_err()
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_real_data_directory_is_removed_with_its_contents() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("multicore");
        std::fs::create_dir_all(root.join("runtime/runtime-00000000000000000001")).unwrap();
        std::fs::write(root.join("preferences.json"), b"{}").unwrap();
        remove_with_retry(&root, 3, Duration::ZERO, |path| {
            std::fs::remove_dir_all(path)
        })
        .unwrap();
        assert!(!root.exists());
        assert!(parent.path().exists());
    }

    #[test]
    fn the_relaunch_uses_the_appimage_file_only_on_linux() {
        let exe = PathBuf::from("/tmp/.mount_MultiC/usr/lib/multicore/multicore");
        let from_appimage =
            launcher_from(Some(OsStr::new("/home/me/MultiCore.AppImage")), exe.clone()).unwrap();
        if cfg!(target_os = "linux") {
            assert_eq!(from_appimage, PathBuf::from("/home/me/MultiCore.AppImage"));
        } else {
            assert_eq!(from_appimage, exe);
        }
        assert_eq!(
            launcher_from(Some(OsStr::new("relative.AppImage")), exe.clone()).unwrap(),
            exe
        );
        assert_eq!(launcher_from(None, exe.clone()).unwrap(), exe);
    }

    #[test]
    fn the_reset_runs_before_the_app_claims_its_single_instance() {
        let main = include_str!("main.rs");
        let reset = main
            .find("data_reset::run_pending_reset(")
            .expect("main handles --reset-data");
        let claim = main
            .find("single_instance::claim_or_forward(&activation)")
            .expect("app claims its instance");
        let entry = main.find("fn main()").expect("main");
        assert!(entry < reset && reset < claim);
        assert!(main.contains("wire_data_reset(&ui);"));
        let slint = include_str!("../ui/app.slint");
        assert!(slint.contains("callback reset-all-data();"));
        assert!(slint.contains("\"Удалить все данные\""));
    }

    #[test]
    fn the_reset_flag_is_not_a_valid_launch_argument() {
        assert!(crate::parse_launch_arguments([std::ffi::OsString::from(RESET_FLAG)]).is_err());
    }
}
