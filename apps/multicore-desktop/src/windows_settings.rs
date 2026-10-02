//! Windows lifecycle settings that belong to the signed-in user.
//!
//! Command construction and classification are platform-neutral on purpose. Only
//! the small registry adapter at the bottom of this file is Windows-specific.

use std::io;
use std::path::{Path, PathBuf};

#[cfg_attr(not(windows), allow(dead_code))]
pub const RUN_VALUE_NAME: &str = "MultiCore";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchAtSignInState {
    Disabled,
    Enabled,
    Stale { stored_command: String },
}

pub fn build_launch_command(executable: &Path) -> io::Result<String> {
    if executable.as_os_str().is_empty()
        || !executable.is_absolute()
        || executable.file_name().is_none()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch executable must be a non-root absolute path",
        ));
    }

    let executable = executable.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch executable path must be valid Unicode",
        )
    })?;

    if executable.contains(['\0', '\r', '\n', '"']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch executable path contains unsafe command characters",
        ));
    }

    Ok(format!("\"{executable}\" --background"))
}

#[cfg(windows)]
fn build_protocol_command(executable: &Path) -> io::Result<String> {
    let launch = build_launch_command(executable)?;
    let quoted_executable = launch
        .strip_suffix(" --background")
        .expect("launch command has its fixed background suffix");
    Ok(format!("{quoted_executable} \"%1\""))
}

#[cfg(windows)]
fn is_installed_executable(executable: &Path) -> bool {
    let Some(current) = executable.parent() else {
        return false;
    };
    let Some(installation) = current.parent() else {
        return false;
    };
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("MultiCore.exe"))
        && current
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("current"))
        && installation.join("unins000.exe").is_file()
        && installation.join("unins000.dat").is_file()
}

/// Repairs registrations lost by an older installation or an in-place updater.
/// Portable copies never claim the system-wide URL scheme for this user.
#[cfg(windows)]
pub fn ensure_url_protocol_registration() -> io::Result<()> {
    let executable = launch_executable(&std::env::current_exe()?);
    if !is_installed_executable(&executable) {
        return Ok(());
    }
    let command = build_protocol_command(&executable)?;
    registry::write_protocol_registration(&executable, &command)
}

/// Renames this install's per-user Apps & Features entry and the user's shortcuts that the
/// 1.1.1 installer named "MultiCore" (an in-app update replaces only `current`). Runs on its
/// own thread; cheap once migrated. The service renames the all-users ones.
#[cfg(windows)]
pub fn migrate_legacy_brand() {
    use multicore_release::brand;
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    if !is_installed_executable(&executable) {
        return;
    }
    let Some(root) = executable
        .parent()
        .and_then(brand::install_root_of_current)
        .map(Path::to_path_buf)
    else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("multicore-branding".into())
        .spawn(move || {
            for line in brand::migrate_install(brand::Scope::User, &root, env!("CARGO_PKG_VERSION"))
            {
                eprintln!("branding: {line}");
            }
        });
}

/// The executable autostart should start: the running one, spelled the plain way. After an
/// in-app update the updater starts the new client from a canonicalized `\\?\` path, and
/// `current_exe` then reports that spelling.
pub fn launch_executable(current_executable: &Path) -> PathBuf {
    multicore_release::brand::without_verbatim_prefix(current_executable)
}

/// `<install>` when `executable` is the installed `<install>\current\MultiCore.exe`.
#[cfg(windows)]
fn install_root_of(executable: &Path) -> Option<PathBuf> {
    if !is_installed_executable(executable) {
        return None;
    }
    executable
        .parent()
        .and_then(multicore_release::brand::install_root_of_current)
        .map(launch_executable)
}

/// Autostart is on when the stored command starts this app: the running executable in any
/// spelling (`\\?\`, case, `/`, missing `--background`), or, for an installed copy, the
/// `MultiCore.exe` of an earlier layout or leftover inside the same install. Only a command
/// that starts something else is stale; it is shown, never deleted or rewritten.
pub fn classify_launch_command(
    stored_command: Option<&str>,
    current_executable: &Path,
    install_root: Option<&Path>,
) -> io::Result<LaunchAtSignInState> {
    build_launch_command(&launch_executable(current_executable))?;

    let Some(stored_command) = stored_command else {
        return Ok(LaunchAtSignInState::Disabled);
    };

    match parse_launch_command(stored_command) {
        Some(stored_executable)
            if starts_this_app(&stored_executable, current_executable, install_root) =>
        {
            Ok(LaunchAtSignInState::Enabled)
        }
        _ => Ok(stale(stored_command)),
    }
}

/// The command to store instead of `stored_command` when autostart is on for this app but
/// is not spelled the way it is written now. `None`: off, foreign, or already exact.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn repaired_launch_command(
    stored_command: Option<&str>,
    current_executable: &Path,
    install_root: Option<&Path>,
) -> io::Result<Option<String>> {
    let expected = build_launch_command(&launch_executable(current_executable))?;
    let state = classify_launch_command(stored_command, current_executable, install_root)?;
    Ok(match (state, stored_command) {
        (LaunchAtSignInState::Enabled, Some(stored)) if stored != expected => Some(expected),
        _ => None,
    })
}

/// The executable of `"<path>" --background`, `"<path>"`, or the same unquoted.
fn parse_launch_command(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    let executable = command
        .strip_suffix(" --background")
        .map(str::trim_end)
        .unwrap_or(command);
    let inner = match executable.strip_prefix('"') {
        Some(quoted) => quoted.strip_suffix('"')?,
        None => executable,
    };
    if inner.is_empty() || inner.contains(['\0', '\r', '\n', '"']) {
        return None;
    }

    let path = PathBuf::from(inner);
    (path.is_absolute() && path.file_name().is_some()).then_some(path)
}

fn starts_this_app(stored: &Path, current: &Path, install_root: Option<&Path>) -> bool {
    if path_key(stored) == path_key(current) || paths_resolve_to_same_file(stored, current) {
        return true;
    }
    let (Some(root), Some(name), Some(current_name)) =
        (install_root, stored.file_name(), current.file_name())
    else {
        return false;
    };
    same_name(name, current_name)
        && stored
            .to_str()
            .is_some_and(|stored| multicore_release::brand::path_is_within(stored, root))
}

/// How a path compares: Windows paths ignore case, `\\?\` and `/` versus `\`.
fn path_key(path: &Path) -> String {
    let plain = launch_executable(path);
    let text = plain.to_string_lossy();
    if cfg!(windows) {
        text.replace('/', "\\").to_lowercase()
    } else {
        text.into_owned()
    }
}

fn same_name(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn paths_resolve_to_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn stale(stored_command: &str) -> LaunchAtSignInState {
    LaunchAtSignInState::Stale {
        stored_command: stored_command.to_owned(),
    }
}

#[cfg(windows)]
pub fn query_launch_at_sign_in() -> io::Result<LaunchAtSignInState> {
    let current_executable = std::env::current_exe()?;
    classify_launch_command(
        registry::read_run_value()?.as_deref(),
        &current_executable,
        install_root_of(&current_executable).as_deref(),
    )
}

/// Startup self-heal: an autostart value that starts this app in another spelling (`\\?\`
/// written by a client the updater relaunched, an earlier layout of this install) is
/// rewritten to the plain command for the running executable. The value keeps its name, so
/// the user's Task Manager choice (`StartupApproved\Run\MultiCore`) stays as it was. Off and
/// foreign values are left alone. True when the value was rewritten.
#[cfg(windows)]
pub fn repair_launch_at_sign_in() -> io::Result<bool> {
    let current_executable = std::env::current_exe()?;
    let Some(command) = repaired_launch_command(
        registry::read_run_value()?.as_deref(),
        &current_executable,
        install_root_of(&current_executable).as_deref(),
    )?
    else {
        return Ok(false);
    };
    registry::write_run_value(&command)?;
    Ok(true)
}

#[cfg(target_os = "linux")]
pub fn query_launch_at_sign_in() -> io::Result<LaunchAtSignInState> {
    autostart::query_in(&autostart::entry_path()?, &autostart::launcher()?)
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn query_launch_at_sign_in() -> io::Result<LaunchAtSignInState> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launch at sign-in is not supported on this platform",
    ))
}

#[cfg(windows)]
pub fn set_launch_at_sign_in(enabled: bool) -> io::Result<LaunchAtSignInState> {
    let current_executable = std::env::current_exe()?;
    apply_launch_at_sign_in_change(
        enabled,
        &current_executable,
        install_root_of(&current_executable).as_deref(),
        registry::write_run_value,
        registry::delete_run_value,
        registry::read_run_value,
    )
}

#[cfg(target_os = "linux")]
pub fn set_launch_at_sign_in(enabled: bool) -> io::Result<LaunchAtSignInState> {
    autostart::apply_in(&autostart::entry_path()?, &autostart::launcher()?, enabled)
}

/// The installed app took over from an AppImage (see `appimage_handoff`): an autostart entry
/// that starts exactly that AppImage now starts the installed app, so the one autostart keeps
/// working and stays shown as on. Any other entry is left alone. True when it was rewritten.
#[cfg(target_os = "linux")]
pub fn adopt_launch_at_sign_in_from(appimage: &Path) -> io::Result<bool> {
    autostart::adopt_in(
        &autostart::entry_path()?,
        appimage,
        &std::env::current_exe()?,
    )
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn set_launch_at_sign_in(_enabled: bool) -> io::Result<LaunchAtSignInState> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "launch at sign-in is not supported on this platform",
    ))
}

#[cfg(target_os = "linux")]
mod autostart {
    use super::{
        LaunchAtSignInState, apply_launch_at_sign_in_change, build_launch_command,
        classify_launch_command,
    };
    use std::ffi::OsStr;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};

    const ENTRY_NAME: &str = "multicore.desktop";
    const MAX_ENTRY_BYTES: u64 = 64 * 1024;
    const EXEC_RESERVED: [char; 4] = ['`', '$', '\\', '%'];

    pub(super) fn entry_path() -> io::Result<PathBuf> {
        Ok(multicore_platform::dirs::user_config_root()?
            .join("autostart")
            .join(ENTRY_NAME))
    }

    pub(super) fn launcher() -> io::Result<PathBuf> {
        Ok(launcher_from(
            std::env::var_os("APPIMAGE").as_deref(),
            &std::env::current_exe()?,
        ))
    }

    pub(super) fn launcher_from(appimage: Option<&OsStr>, current_exe: &Path) -> PathBuf {
        appimage
            .map(Path::new)
            .filter(|image| image.is_absolute() && image.is_file())
            .map_or_else(|| current_exe.to_path_buf(), Path::to_path_buf)
    }

    pub(super) fn desktop_entry(command: &str) -> String {
        format!(
            "[Desktop Entry]\nType=Application\nName=MULTICORE\nComment=MULTICORE network client\nExec={command}\nIcon=multicore\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        )
    }

    pub(super) fn exec_from_desktop_entry(entry: &str) -> Option<String> {
        let mut in_main_group = false;
        let mut exec = None;
        for line in entry.lines().map(str::trim) {
            if line.starts_with('[') {
                in_main_group = line == "[Desktop Entry]";
                continue;
            }
            if !in_main_group {
                continue;
            }
            match line
                .split_once('=')
                .map(|(key, value)| (key.trim(), value.trim()))
            {
                Some(("Hidden", "true")) | Some(("X-GNOME-Autostart-enabled", "false")) => {
                    return None;
                }
                Some(("Exec", value)) if exec.is_none() => exec = Some(value.to_owned()),
                _ => {}
            }
        }
        exec
    }

    fn read_exec(entry: &Path) -> io::Result<Option<String>> {
        let file = match std::fs::File::open(entry) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(MAX_ENTRY_BYTES).read_to_string(&mut text)?;
        Ok(exec_from_desktop_entry(&text))
    }

    fn write_entry(entry: &Path, command: &str) -> io::Result<()> {
        let directory = entry
            .parent()
            .ok_or_else(|| io::Error::other("autostart entry has no directory"))?;
        std::fs::create_dir_all(directory)?;
        let temporary = directory.join(format!(".{ENTRY_NAME}.tmp"));
        std::fs::write(&temporary, desktop_entry(command))?;
        std::fs::rename(&temporary, entry).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
    }

    fn delete_entry(entry: &Path) -> io::Result<()> {
        match std::fs::remove_file(entry) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    fn require_exec_safe(executable: &Path) -> io::Result<()> {
        let command = build_launch_command(executable)?;
        if command.contains(EXEC_RESERVED) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "launch executable path needs Desktop Entry escaping",
            ));
        }
        Ok(())
    }

    pub(super) fn adopt_in(entry: &Path, previous: &Path, executable: &Path) -> io::Result<bool> {
        let Some(stored) = read_exec(entry)? else {
            return Ok(false);
        };
        if stored != build_launch_command(previous)? {
            return Ok(false);
        }
        require_exec_safe(executable)?;
        write_entry(entry, &build_launch_command(executable)?)?;
        Ok(true)
    }

    pub(super) fn query_in(entry: &Path, executable: &Path) -> io::Result<LaunchAtSignInState> {
        classify_launch_command(read_exec(entry)?.as_deref(), executable, None)
    }

    pub(super) fn apply_in(
        entry: &Path,
        executable: &Path,
        enabled: bool,
    ) -> io::Result<LaunchAtSignInState> {
        if enabled {
            require_exec_safe(executable)?;
        }
        apply_launch_at_sign_in_change(
            enabled,
            executable,
            None,
            |command| write_entry(entry, command),
            || delete_entry(entry),
            || read_exec(entry),
        )
    }
}

fn apply_launch_at_sign_in_change<Write, Delete, Read>(
    enabled: bool,
    current_executable: &Path,
    install_root: Option<&Path>,
    mut write: Write,
    mut delete: Delete,
    mut read: Read,
) -> io::Result<LaunchAtSignInState>
where
    Write: FnMut(&str) -> io::Result<()>,
    Delete: FnMut() -> io::Result<()>,
    Read: FnMut() -> io::Result<Option<String>>,
{
    if enabled {
        write(&build_launch_command(&launch_executable(
            current_executable,
        ))?)?;
    } else {
        delete()?;
    }

    let observed = classify_launch_command(read()?.as_deref(), current_executable, install_root)?;
    let verified = matches!(
        (enabled, &observed),
        (true, LaunchAtSignInState::Enabled) | (false, LaunchAtSignInState::Disabled)
    );
    if verified {
        Ok(observed)
    } else {
        Err(io::Error::other(
            "launch-at-sign-in registry change could not be verified",
        ))
    }
}

#[cfg(windows)]
mod registry {
    use super::RUN_VALUE_NAME;
    use std::ffi::c_void;
    use std::io;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegGetValueW, RegSetValueExW,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const PROTOCOL_KEY: &str = "Software\\Classes\\multicore";
    const MAX_VALUE_BYTES: u32 = 64 * 1024;

    pub(super) fn read_run_value() -> io::Result<Option<String>> {
        let subkey = wide(RUN_KEY);
        let value_name = wide(RUN_VALUE_NAME);
        let mut byte_len = 0_u32;

        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                null_mut(),
                &mut byte_len,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        win32_result(status)?;
        if byte_len > MAX_VALUE_BYTES || !byte_len.is_multiple_of(2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid launch-at-sign-in registry value size",
            ));
        }

        let mut buffer = vec![0_u16; (byte_len as usize / 2).max(1)];
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value_name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut byte_len,
            )
        };
        win32_result(status)?;
        buffer.truncate(byte_len as usize / 2);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }

        String::from_utf16(&buffer)
            .map(Some)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "registry value is not UTF-16"))
    }

    pub(super) fn write_run_value(command: &str) -> io::Result<()> {
        write_string_value(RUN_KEY, RUN_VALUE_NAME, command)
    }

    pub(super) fn write_protocol_registration(
        executable: &std::path::Path,
        command: &str,
    ) -> io::Result<()> {
        write_string_value(PROTOCOL_KEY, "", "URL:MULTICORE Protocol")?;
        write_string_value(PROTOCOL_KEY, "URL Protocol", "")?;
        write_string_value(
            &format!("{PROTOCOL_KEY}\\DefaultIcon"),
            "",
            &format!("{},0", executable.display()),
        )?;
        write_string_value(
            &format!("{PROTOCOL_KEY}\\shell\\open\\command"),
            "",
            command,
        )
    }

    fn write_string_value(subkey: &str, value_name: &str, value: &str) -> io::Result<()> {
        let subkey = wide(subkey);
        let mut key: HKEY = null_mut();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                null(),
                &mut key,
                null_mut(),
            )
        };
        win32_result(status)?;
        let key = RegistryKey(key);
        let value_name = wide(value_name);
        let value = wide(value);
        let bytes = value
            .len()
            .checked_mul(2)
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "registry value is too large")
            })?;
        let status = unsafe {
            RegSetValueExW(
                key.0,
                value_name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast::<u8>(),
                bytes,
            )
        };
        win32_result(status)
    }

    pub(super) fn delete_run_value() -> io::Result<()> {
        let subkey = wide(RUN_KEY);
        let value_name = wide(RUN_VALUE_NAME);
        let status =
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, subkey.as_ptr(), value_name.as_ptr()) };
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            win32_result(status)
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn win32_result(status: u32) -> io::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{
        LaunchAtSignInState, apply_launch_at_sign_in_change, build_launch_command,
        build_protocol_command, classify_launch_command, is_installed_executable,
        launch_executable, repaired_launch_command,
    };
    use std::cell::RefCell;
    use std::path::Path;
    use std::rc::Rc;

    #[cfg(windows)]
    #[test]
    fn launch_command_is_exactly_quoted_and_backgrounded() {
        assert_eq!(
            build_launch_command(Path::new(r"C:\Program Files\MultiCore\MultiCore.exe")).unwrap(),
            r#""C:\Program Files\MultiCore\MultiCore.exe" --background"#
        );
    }

    #[test]
    fn installed_protocol_command_is_quoted_and_portable_copy_is_not_registered() {
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("current");
        std::fs::create_dir(&current).unwrap();
        let executable = current.join("MultiCore.exe");
        std::fs::write(&executable, b"app").unwrap();

        assert!(!is_installed_executable(&executable));
        std::fs::write(root.path().join("unins000.exe"), b"uninstaller").unwrap();
        std::fs::write(root.path().join("unins000.dat"), b"metadata").unwrap();
        assert!(is_installed_executable(&executable));
        assert_eq!(
            build_protocol_command(&executable).unwrap(),
            format!("\"{}\" \"%1\"", executable.display())
        );
    }

    #[cfg(windows)]
    #[test]
    fn commands_that_start_this_executable_are_enabled_and_others_stale() {
        let executable = Path::new(r"C:\Apps\MultiCore.exe");
        for ours in [
            r#""C:\Apps\MultiCore.exe" --background"#,
            r#""C:\Apps\MultiCore.exe""#,
            r#"C:\Apps\MultiCore.exe --background"#,
            r#" "C:\Apps\MultiCore.exe" --background "#,
        ] {
            assert_eq!(
                classify_launch_command(Some(ours), executable, None).unwrap(),
                LaunchAtSignInState::Enabled,
                "{ours}"
            );
        }

        for stale in [
            r#""C:\Apps\MultiCore.exe" --background --extra"#,
            r#""C:\Apps\MultiCore.exe --background"#,
            r#""C:\Other\MultiCore.exe" --background"#,
        ] {
            assert!(
                matches!(
                    classify_launch_command(Some(stale), executable, None).unwrap(),
                    LaunchAtSignInState::Stale { .. }
                ),
                "{stale}"
            );
        }
    }

    const INSTALLED: &str = r"C:\Program Files\MultiCore\current\MultiCore.exe";
    const VERBATIM: &str = r"\\?\C:\Program Files\MultiCore\current\MultiCore.exe";
    const ROOT: &str = r"C:\Program Files\MultiCore";
    const CANONICAL: &str = r#""C:\Program Files\MultiCore\current\MultiCore.exe" --background"#;

    /// The updater relaunches the new client from a canonicalized (`\\?\`) path, while the
    /// installer, the shortcuts and an earlier toggle use the plain one. Either spelling of
    /// the same executable is the user's autostart and must read as on.
    #[test]
    fn every_spelling_of_the_running_executable_is_enabled() {
        for (stored, running) in [
            (CANONICAL, VERBATIM),
            (
                r#""\\?\C:\Program Files\MultiCore\current\MultiCore.exe" --background"#,
                INSTALLED,
            ),
            (
                r#""\\?\C:\Program Files\MultiCore\current\MultiCore.exe" --background"#,
                VERBATIM,
            ),
            (
                r#""c:\program files\multicore\CURRENT\multicore.exe" --background"#,
                INSTALLED,
            ),
            (
                r#""C:/Program Files/MultiCore/current/MultiCore.exe" --background"#,
                INSTALLED,
            ),
            (
                r#""C:\Program Files\MultiCore\current\MultiCore.exe""#,
                INSTALLED,
            ),
            (
                r#"  "C:\Program Files\MultiCore\current\MultiCore.exe" --background "#,
                INSTALLED,
            ),
        ] {
            for root in [None, Some(Path::new(ROOT))] {
                assert_eq!(
                    classify_launch_command(Some(stored), Path::new(running), root).unwrap(),
                    LaunchAtSignInState::Enabled,
                    "{stored} for {running}"
                );
            }
        }
    }

    /// Whatever spelling is stored, startup rewrites it to the plain command for the running
    /// executable; the value keeps its name, so Task Manager's StartupApproved state stays.
    #[test]
    fn startup_repairs_any_spelling_to_the_plain_command() {
        let root = Some(Path::new(ROOT));
        for running in [INSTALLED, VERBATIM] {
            for stored in [
                r#""\\?\C:\Program Files\MultiCore\current\MultiCore.exe" --background"#,
                r#""c:\program files\multicore\current\multicore.exe" --background"#,
                r#""C:\Program Files\MultiCore\current\MultiCore.exe""#,
                // Earlier layouts and leftovers of this install.
                r#""C:\Program Files\MultiCore\MultiCore.exe" --background"#,
                r#""C:\Program Files\MultiCore\.multicore-previous-1.1.4-77\MultiCore.exe" --background"#,
            ] {
                assert_eq!(
                    repaired_launch_command(Some(stored), Path::new(running), root)
                        .unwrap()
                        .as_deref(),
                    Some(CANONICAL),
                    "{stored} for {running}"
                );
            }
            assert_eq!(
                repaired_launch_command(Some(CANONICAL), Path::new(running), root).unwrap(),
                None,
                "the canonical command is left alone"
            );
            assert_eq!(
                repaired_launch_command(None, Path::new(running), root).unwrap(),
                None,
                "autostart that is off stays off"
            );
        }
    }

    #[test]
    fn foreign_commands_are_stale_and_never_rewritten() {
        let root = Some(Path::new(ROOT));
        for stored in [
            r#""D:\Portable\MultiCore\MultiCore.exe" --background"#,
            r#""C:\Program Files\MultiCore2\current\MultiCore.exe" --background"#,
            r#""C:\Program Files\MultiCore\current\Other.exe" --background"#,
            r#""C:\Program Files\MultiCore\current\MultiCore.exe" --background --extra"#,
            "not a command",
        ] {
            assert!(
                matches!(
                    classify_launch_command(Some(stored), Path::new(INSTALLED), root).unwrap(),
                    LaunchAtSignInState::Stale { .. }
                ),
                "{stored}"
            );
            assert_eq!(
                repaired_launch_command(Some(stored), Path::new(INSTALLED), root).unwrap(),
                None,
                "{stored}"
            );
        }
        // A portable copy only owns its own executable, not a folder around it.
        assert!(matches!(
            classify_launch_command(
                Some(r#""C:\Program Files\MultiCore\MultiCore.exe" --background"#),
                Path::new(INSTALLED),
                None
            )
            .unwrap(),
            LaunchAtSignInState::Stale { .. }
        ));
    }

    #[test]
    fn turning_on_from_a_verbatim_launch_writes_the_plain_command() {
        let stored = Rc::new(RefCell::new(None::<String>));
        let write_store = stored.clone();
        let read_store = stored.clone();
        let state = apply_launch_at_sign_in_change(
            true,
            &launch_executable(Path::new(VERBATIM)),
            Some(Path::new(ROOT)),
            move |command| {
                *write_store.borrow_mut() = Some(command.to_owned());
                Ok(())
            },
            || Ok(()),
            move || Ok(read_store.borrow().clone()),
        )
        .unwrap();
        assert_eq!(state, LaunchAtSignInState::Enabled);
        assert_eq!(stored.borrow().as_deref(), Some(CANONICAL));
    }

    #[test]
    fn launch_executable_drops_only_the_verbatim_prefix() {
        assert_eq!(launch_executable(Path::new(VERBATIM)), Path::new(INSTALLED));
        assert_eq!(
            launch_executable(Path::new(INSTALLED)),
            Path::new(INSTALLED)
        );
    }

    #[cfg(windows)]
    #[test]
    fn persistence_is_reread_and_verified_without_real_registry_access() {
        let executable = Path::new(r"C:\Apps\MultiCore.exe");
        let stored = Rc::new(RefCell::new(None::<String>));
        let write_store = stored.clone();
        let delete_store = stored.clone();
        let read_store = stored.clone();

        let enabled = apply_launch_at_sign_in_change(
            true,
            executable,
            None,
            move |command| {
                *write_store.borrow_mut() = Some(command.to_owned());
                Ok(())
            },
            move || {
                *delete_store.borrow_mut() = None;
                Ok(())
            },
            move || Ok(read_store.borrow().clone()),
        )
        .unwrap();
        assert_eq!(enabled, LaunchAtSignInState::Enabled);

        let ignored_write = apply_launch_at_sign_in_change(
            true,
            executable,
            None,
            |_| Ok(()),
            || Ok(()),
            || Ok(None),
        );
        assert!(ignored_write.is_err());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::LaunchAtSignInState;
    use super::autostart::{
        adopt_in, apply_in, desktop_entry, exec_from_desktop_entry, launcher_from, query_in,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn desktop_entry_round_trips_the_exact_background_command() {
        let command = "\"/usr/lib/multicore/multicore\" --background";
        let entry = desktop_entry(command);
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("\nType=Application\n"));
        assert!(entry.contains("\nName=MULTICORE\n"));
        assert_eq!(exec_from_desktop_entry(&entry).as_deref(), Some(command));
    }

    #[test]
    fn entries_disabled_by_the_desktop_or_outside_the_main_group_are_ignored() {
        let base = "[Desktop Entry]\nType=Application\nExec=\"/opt/mc\" --background\n";
        assert!(exec_from_desktop_entry(base).is_some());
        assert_eq!(
            exec_from_desktop_entry(&format!("{base}Hidden=true\n")),
            None
        );
        assert_eq!(
            exec_from_desktop_entry(&format!("{base}X-GNOME-Autostart-enabled=false\n")),
            None
        );
        assert_eq!(
            exec_from_desktop_entry("[Desktop Action x]\nExec=\"/opt/mc\" --background\n"),
            None
        );
        assert_eq!(
            exec_from_desktop_entry("Exec=\"/opt/mc\" --background\n"),
            None
        );
    }

    #[test]
    fn appimage_path_is_preferred_over_the_mounted_executable() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("MultiCore.AppImage");
        std::fs::write(&image, b"image").unwrap();
        let mounted = Path::new("/tmp/.mount_MultiXYZ/usr/lib/multicore/multicore");
        assert_eq!(launcher_from(Some(image.as_os_str()), mounted), image);
        assert_eq!(
            launcher_from(Some("relative.AppImage".as_ref()), mounted),
            PathBuf::from(mounted)
        );
        assert_eq!(launcher_from(None, mounted), PathBuf::from(mounted));
    }

    #[test]
    fn enabling_and_disabling_is_written_atomically_and_verified() {
        let temp = tempfile::tempdir().unwrap();
        let entry = temp.path().join("autostart").join("multicore.desktop");
        let executable = temp.path().join("Multi Core").join("multicore");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"elf").unwrap();

        assert_eq!(
            query_in(&entry, &executable).unwrap(),
            LaunchAtSignInState::Disabled
        );
        assert_eq!(
            apply_in(&entry, &executable, true).unwrap(),
            LaunchAtSignInState::Enabled
        );
        assert!(
            std::fs::read_to_string(&entry)
                .unwrap()
                .contains(&format!("Exec=\"{}\" --background", executable.display()))
        );
        assert_eq!(
            query_in(&entry, &executable).unwrap(),
            LaunchAtSignInState::Enabled
        );
        assert!(
            std::fs::read_dir(entry.parent().unwrap())
                .unwrap()
                .all(|file| file.unwrap().file_name() == "multicore.desktop")
        );
        assert_eq!(
            apply_in(&entry, &executable, false).unwrap(),
            LaunchAtSignInState::Disabled
        );
        assert!(!entry.exists());
        assert_eq!(
            apply_in(&entry, &executable, false).unwrap(),
            LaunchAtSignInState::Disabled
        );
    }

    #[test]
    fn a_foreign_command_is_stale_and_unsafe_paths_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let entry = temp.path().join("multicore.desktop");
        let executable = temp.path().join("multicore");
        std::fs::write(&executable, b"elf").unwrap();
        std::fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nExec=\"/old/place/multicore\" --background\n",
        )
        .unwrap();
        assert!(matches!(
            query_in(&entry, &executable).unwrap(),
            LaunchAtSignInState::Stale { .. }
        ));
        for unsafe_name in ["cost$", "tick`", "back\\slash", "field%u"] {
            let unsafe_executable = temp.path().join(unsafe_name);
            assert!(
                apply_in(&entry, &unsafe_executable, true).is_err(),
                "{unsafe_name}"
            );
        }
    }

    #[test]
    fn an_autostart_of_the_handed_off_appimage_is_adopted_by_the_installed_app() {
        let temp = tempfile::tempdir().unwrap();
        let entry = temp.path().join("autostart").join("multicore.desktop");
        let image = temp.path().join("MultiCore-1.1.1-x86_64.AppImage");
        let installed = temp.path().join("usr-lib-multicore").join("multicore");
        let other = temp.path().join("Other.AppImage");
        for file in [&image, &installed, &other] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"elf").unwrap();
        }

        // No autostart: nothing to adopt, nothing written.
        assert!(!adopt_in(&entry, &image, &installed).unwrap());
        assert!(!entry.exists());

        // Another launcher's entry is not ours to change.
        apply_in(&entry, &other, true).unwrap();
        assert!(!adopt_in(&entry, &image, &installed).unwrap());
        assert_eq!(
            query_in(&entry, &other).unwrap(),
            LaunchAtSignInState::Enabled
        );

        // The AppImage's own entry now starts the installed app and reads as enabled.
        apply_in(&entry, &image, true).unwrap();
        assert!(adopt_in(&entry, &image, &installed).unwrap());
        assert_eq!(
            query_in(&entry, &installed).unwrap(),
            LaunchAtSignInState::Enabled
        );
        assert!(!adopt_in(&entry, &image, &installed).unwrap(), "idempotent");
    }
}
