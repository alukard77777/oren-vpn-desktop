//! The product name is written MULTICORE wherever a user sees it. Installs from 1.1.1 and
//! earlier were registered as "MultiCore", and an in-app update replaces only the `current`
//! folder, so the installed copy renames what its installer left behind: the Apps & Features
//! entry and the Start menu / desktop shortcuts. Internal names (folders, the service name,
//! the Run value, AppId) stay "MultiCore".
//!
//! The decisions are pure functions; the Windows side only reads and applies them, best
//! effort and only for what belongs to this install.

use std::path::Path;

pub const PRODUCT_NAME: &str = "MULTICORE";
pub const LEGACY_PRODUCT_NAME: &str = "MultiCore";
/// Must differ from the service name "MultiCore" by more than case: the SCM shows the service
/// name instead of a display name that matches it case-insensitively.
pub const SERVICE_DISPLAY_NAME: &str = "MULTICORE Service";
pub const SERVICE_DESCRIPTION: &str = "MULTICORE: ядра Mihomo и Xray для мгновенного подключения";
/// The 1.1.1 description, the only one the startup migration replaces: since 1.1.4 the
/// installer writes it in the Windows UI language, and an English one must stay English.
const LEGACY_SERVICE_DESCRIPTION: &str =
    "MultiCore: ядра Mihomo и Xray для мгновенного подключения";
/// Inno Setup's uninstall entry for AppId {D95AB954-248C-44C9-9E3A-952398802A2D}.
pub const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{D95AB954-248C-44C9-9E3A-952398802A2D}_is1";
pub const SHORTCUT_FILE_NAME: &str = "MULTICORE.lnk";
/// `HKLM\Software\Classes\multicore`, written by the service-mode installer.
pub const PROTOCOL_KEY: &str = r"Software\Classes\multicore";
pub const PROTOCOL_DESCRIPTION: &str = "URL:MULTICORE Protocol";
const LEGACY_PROTOCOL_DESCRIPTION: &str = "URL:MultiCore Protocol";

/// `<install>` for an executable's `<install>\current` folder; `None` for a portable copy.
pub fn install_root_of_current(current: &Path) -> Option<&Path> {
    current
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.eq_ignore_ascii_case("current"))?;
    current.parent().filter(|root| !root.as_os_str().is_empty())
}

fn normalize(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

/// `path` is `root` or lies beneath it (case-insensitive, `\\?\` and trailing `\` ignored).
pub fn path_is_within(path: &str, root: &Path) -> bool {
    let Some(root) = root.to_str() else {
        return false;
    };
    let (path, root) = (normalize(path), normalize(root));
    !root.is_empty() && (path == root || path.starts_with(&format!("{root}\\")))
}

/// What this install's uninstall entry holds now (`None`: absent or unreadable).
#[derive(Clone, Copy, Debug, Default)]
pub struct UninstallEntry<'a> {
    pub install_location: Option<&'a str>,
    pub display_name: Option<&'a str>,
    pub publisher: Option<&'a str>,
    pub display_version: Option<&'a str>,
    /// The DWORDs Inno Setup writes next to `DisplayVersion`, when present.
    pub major_version: Option<u32>,
    pub minor_version: Option<u32>,
    pub version_major: Option<u32>,
    pub version_minor: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryValue {
    Text(String),
    Number(u32),
}

impl std::fmt::Display for EntryValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => formatter.write_str(text),
            Self::Number(number) => write!(formatter, "{number}"),
        }
    }
}

/// `(major, minor)` of a version such as `1.1.2` or `1.2.0-rc.1`.
pub fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split(['-', '+']).next()?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Values of the uninstall entry to rewrite, as (value name, new data). Only the entry of
/// this install (its `InstallLocation`) changes: names still spelled "MultiCore", and the
/// version an in-app update moved past (`DisplayVersion`, Inno's version DWORDs and the
/// version at the end of `DisplayName`). Empty once both are current.
pub fn uninstall_entry_updates(
    entry: &UninstallEntry<'_>,
    install_root: &Path,
    version: &str,
) -> Vec<(&'static str, EntryValue)> {
    let Some(location) = entry.install_location else {
        return Vec::new();
    };
    if normalize(location) != install_root.to_str().map(normalize).unwrap_or_default() {
        return Vec::new();
    }
    let mut updates = Vec::new();
    if let Some(name) = entry.display_name {
        let mut renamed = match name.strip_prefix(LEGACY_PRODUCT_NAME) {
            Some(rest) => format!("{PRODUCT_NAME}{rest}"),
            None => name.to_owned(),
        };
        // Inno's AppVerName ("MULTICORE version 1.1.1") ends with the installed version.
        if let Some(installed) = entry
            .display_version
            .filter(|installed| !installed.is_empty() && *installed != version)
            && let Some(head) = renamed.strip_suffix(installed)
            && head.ends_with(' ')
        {
            renamed = format!("{head}{version}");
        }
        if renamed != name {
            updates.push(("DisplayName", EntryValue::Text(renamed)));
        }
    }
    if entry.publisher == Some(LEGACY_PRODUCT_NAME) {
        updates.push(("Publisher", EntryValue::Text(PRODUCT_NAME.to_owned())));
    }
    if entry
        .display_version
        .is_some_and(|installed| installed != version)
    {
        updates.push(("DisplayVersion", EntryValue::Text(version.to_owned())));
    }
    if let Some((major, minor)) = major_minor(version) {
        for (name, current, wanted) in [
            ("MajorVersion", entry.major_version, major),
            ("MinorVersion", entry.minor_version, minor),
            ("VersionMajor", entry.version_major, major),
            ("VersionMinor", entry.version_minor, minor),
        ] {
            if current.is_some_and(|current| current != wanted) {
                updates.push((name, EntryValue::Number(wanted)));
            }
        }
    }
    updates
}

/// What the service registration still needs: (display name, description) to set, each
/// `None` when already current. A display name that could not be read is set again; the
/// description only replaces the 1.1.1 one (the installer localizes it).
pub fn service_updates(
    display_name: Option<&str>,
    description: Option<&str>,
) -> (Option<&'static str>, Option<&'static str>) {
    (
        (display_name != Some(SERVICE_DISPLAY_NAME)).then_some(SERVICE_DISPLAY_NAME),
        (description == Some(LEGACY_SERVICE_DESCRIPTION)).then_some(SERVICE_DESCRIPTION),
    )
}

/// The `multicore://` handler's description to set: only the old one, and only when the
/// handler (`"<exe>" "%1"`) starts this install.
pub fn protocol_description_update(
    description: Option<&str>,
    command: Option<&str>,
    install_root: &Path,
) -> Option<&'static str> {
    let executable = command?.strip_prefix('"')?.split('"').next()?;
    (description == Some(LEGACY_PROTOCOL_DESCRIPTION) && path_is_within(executable, install_root))
        .then_some(PROTOCOL_DESCRIPTION)
}

/// The shortcut in a folder listing still named in the old spelling (`MultiCore.lnk`), if any.
pub fn legacy_shortcut_name<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    names
        .into_iter()
        .find(|name| name.eq_ignore_ascii_case(SHORTCUT_FILE_NAME) && *name != SHORTCUT_FILE_NAME)
}

/// The paths a shell link (`.lnk`, MS-SHLLINK) stores for its target.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShellLinkPaths {
    /// LinkInfo LocalBasePath + CommonPathSuffix, when stored as Unicode.
    pub target_unicode: Option<String>,
    /// The same in the ANSI code page of the machine that wrote it.
    pub target_ansi: Option<Vec<u8>>,
    pub working_dir: Option<String>,
}

const HAS_LINK_TARGET_ID_LIST: u32 = 0x1;
const HAS_LINK_INFO: u32 = 0x2;
const HAS_NAME: u32 = 0x4;
const HAS_RELATIVE_PATH: u32 = 0x8;
const HAS_WORKING_DIR: u32 = 0x10;
const IS_UNICODE: u32 = 0x80;
const VOLUME_ID_AND_LOCAL_BASE_PATH: u32 = 0x1;
const SHELL_LINK_HEADER_SIZE: usize = 0x4C;
const SHELL_LINK_CLSID: [u8; 16] = [
    0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset.checked_add(2)?)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset.checked_add(4)?)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn ansi_z(bytes: &[u8], offset: usize) -> Option<Vec<u8>> {
    let tail = bytes.get(offset..)?;
    let end = tail.iter().position(|&b| b == 0)?;
    Some(tail[..end].to_vec())
}

fn unicode_z(bytes: &[u8], offset: usize) -> Option<String> {
    let tail = bytes.get(offset..)?;
    let units: Vec<u16> = tail
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&unit| unit != 0)
        .collect();
    // A string that ran into the end of the buffer had no terminator.
    if units.len() * 2 + 2 > tail.len() {
        return None;
    }
    String::from_utf16(&units).ok()
}

/// Reads the target and working directory of a shell link; `None` when it is not one.
pub fn parse_shell_link(bytes: &[u8]) -> Option<ShellLinkPaths> {
    if u32_at(bytes, 0)? as usize != SHELL_LINK_HEADER_SIZE || bytes.get(4..20)? != SHELL_LINK_CLSID
    {
        return None;
    }
    let flags = u32_at(bytes, 0x14)?;
    let mut offset = SHELL_LINK_HEADER_SIZE;
    if flags & HAS_LINK_TARGET_ID_LIST != 0 {
        offset = offset.checked_add(2 + usize::from(u16_at(bytes, offset)?))?;
    }
    let mut paths = ShellLinkPaths::default();
    if flags & HAS_LINK_INFO != 0 {
        let info = bytes.get(offset..)?;
        let size = u32_at(info, 0)? as usize;
        let header_size = u32_at(info, 4)? as usize;
        let info = info.get(..size)?;
        let info_flags = u32_at(info, 8)?;
        if info_flags & VOLUME_ID_AND_LOCAL_BASE_PATH != 0 {
            let base = u32_at(info, 16)? as usize;
            let suffix = u32_at(info, 24)? as usize;
            let mut ansi = ansi_z(info, base)?;
            if suffix != 0 {
                ansi.extend(ansi_z(info, suffix)?);
            }
            paths.target_ansi = Some(ansi);
            if header_size >= 0x24 {
                let base = u32_at(info, 28)? as usize;
                let suffix = u32_at(info, 32)? as usize;
                if base != 0 {
                    let mut unicode = unicode_z(info, base)?;
                    if suffix != 0 {
                        unicode.push_str(&unicode_z(info, suffix)?);
                    }
                    paths.target_unicode = Some(unicode);
                }
            }
        }
        offset = offset.checked_add(size)?;
    }
    let unicode = flags & IS_UNICODE != 0;
    for (flag, is_working_dir) in [
        (HAS_NAME, false),
        (HAS_RELATIVE_PATH, false),
        (HAS_WORKING_DIR, true),
    ] {
        if flags & flag == 0 {
            continue;
        }
        let count = usize::from(u16_at(bytes, offset)?);
        let width = if unicode { 2 } else { 1 };
        let start = offset + 2;
        let data = bytes.get(start..start.checked_add(count * width)?)?;
        if is_working_dir {
            paths.working_dir = Some(if unicode {
                let units: Vec<u16> = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                String::from_utf16(&units).ok()?
            } else {
                String::from_utf8(data.to_vec()).ok()?
            });
        }
        offset = start + count * width;
    }
    Some(paths)
}

/// The link starts something inside this install. The stored target decides; the working
/// directory only when the target is stored in a code page this check cannot read (a
/// non-ASCII path written as ANSI).
pub fn shell_link_points_into(paths: &ShellLinkPaths, install_root: &Path) -> bool {
    if let Some(target) = &paths.target_unicode {
        return path_is_within(target, install_root);
    }
    if let Some(target) = &paths.target_ansi
        && target.is_ascii()
    {
        return std::str::from_utf8(target)
            .is_ok_and(|target| path_is_within(target, install_root));
    }
    paths
        .working_dir
        .as_deref()
        .is_some_and(|directory| path_is_within(directory, install_root))
}

/// Where the install's entry and shortcuts live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Service mode: HKLM entry, all-users Start menu and public desktop (LocalSystem).
    Machine,
    /// Per-user mode: HKCU entry, the user's Start menu and desktop.
    User,
}

/// Renames this install's Apps & Features entry and shortcuts to MULTICORE and moves the
/// entry's version to `version` (the running build). Best effort and idempotent, cheap when
/// current; returns one line per change or failure for the caller's log.
#[cfg(windows)]
pub fn migrate_install(scope: Scope, install_root: &Path, version: &str) -> Vec<String> {
    windows::migrate_install(scope, install_root, version)
}

#[cfg(windows)]
mod windows {
    use super::{
        EntryValue, PROTOCOL_KEY, SHORTCUT_FILE_NAME, Scope, UNINSTALL_KEY, UninstallEntry,
        legacy_shortcut_name, parse_shell_link, protocol_description_update,
        shell_link_points_into, uninstall_entry_updates,
    };
    use std::{
        ffi::{OsString, c_void},
        io,
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        ptr::{null, null_mut},
    };
    use windows_sys::{
        Win32::{
            Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
            System::{
                Com::CoTaskMemFree,
                Registry::{
                    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE,
                    REG_DWORD, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW,
                    RegOpenKeyExW, RegSetValueExW,
                },
            },
            UI::Shell::{
                FOLDERID_CommonPrograms, FOLDERID_Desktop, FOLDERID_Programs,
                FOLDERID_PublicDesktop, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
            },
        },
        core::GUID,
    };

    const MAX_VALUE_BYTES: u32 = 64 * 1024;
    const MAX_SHORTCUT_BYTES: u64 = 1024 * 1024;

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn open_entry(scope: Scope) -> io::Result<Option<Key>> {
        let root = match scope {
            Scope::Machine => HKEY_LOCAL_MACHINE,
            Scope::User => HKEY_CURRENT_USER,
        };
        open_key(root, UNINSTALL_KEY)
    }

    fn open_key(root: HKEY, subkey: &str) -> io::Result<Option<Key>> {
        let subkey = wide(subkey);
        let mut key: HKEY = null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                root,
                subkey.as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Some(Key(key)))
    }

    fn read_value(key: &Key, name: &str) -> Option<String> {
        let name = wide(name);
        let mut byte_len = 0_u32;
        let status = unsafe {
            RegGetValueW(
                key.0,
                null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                null_mut(),
                &mut byte_len,
            )
        };
        if status != ERROR_SUCCESS || byte_len > MAX_VALUE_BYTES {
            return None;
        }
        let mut buffer = vec![0_u16; (byte_len as usize / 2).max(1)];
        let status = unsafe {
            RegGetValueW(
                key.0,
                null(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut byte_len,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        buffer.truncate(byte_len as usize / 2);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        String::from_utf16(&buffer).ok()
    }

    fn read_dword(key: &Key, name: &str) -> Option<u32> {
        let name = wide(name);
        let mut value = 0_u32;
        let mut byte_len = 4_u32;
        let status = unsafe {
            RegGetValueW(
                key.0,
                null(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&raw mut value).cast::<c_void>(),
                &mut byte_len,
            )
        };
        (status == ERROR_SUCCESS).then_some(value)
    }

    fn write_dword(key: &Key, name: &str, value: u32) -> io::Result<()> {
        let name = wide(name);
        let status = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_DWORD,
                (&raw const value).cast::<u8>(),
                4,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    fn write_value(key: &Key, name: &str, value: &str) -> io::Result<()> {
        let name = wide(name);
        let value = wide(value);
        let bytes = u32::try_from(value.len() * 2)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "value too large"))?;
        let status = unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast::<u8>(),
                bytes,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    fn migrate_entry(scope: Scope, install_root: &Path, version: &str, log: &mut Vec<String>) {
        let key = match open_entry(scope) {
            Ok(Some(key)) => key,
            Ok(None) => return,
            Err(error) => {
                log.push(format!("uninstall entry not opened: {error}"));
                return;
            }
        };
        let (location, display_name, publisher, display_version) = (
            read_value(&key, "InstallLocation"),
            read_value(&key, "DisplayName"),
            read_value(&key, "Publisher"),
            read_value(&key, "DisplayVersion"),
        );
        let entry = UninstallEntry {
            install_location: location.as_deref(),
            display_name: display_name.as_deref(),
            publisher: publisher.as_deref(),
            display_version: display_version.as_deref(),
            major_version: read_dword(&key, "MajorVersion"),
            minor_version: read_dword(&key, "MinorVersion"),
            version_major: read_dword(&key, "VersionMajor"),
            version_minor: read_dword(&key, "VersionMinor"),
        };
        for (name, value) in uninstall_entry_updates(&entry, install_root, version) {
            let written = match &value {
                EntryValue::Text(text) => write_value(&key, name, text),
                EntryValue::Number(number) => write_dword(&key, name, *number),
            };
            match written {
                Ok(()) => log.push(format!("uninstall entry {name} set to {value}")),
                Err(error) => log.push(format!("uninstall entry {name} not set: {error}")),
            }
        }
    }

    fn known_folder(id: &GUID) -> Option<PathBuf> {
        let mut raw = null_mut();
        if unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, null_mut(), &mut raw) } < 0 {
            if !raw.is_null() {
                unsafe { CoTaskMemFree(raw.cast()) };
            }
            return None;
        }
        let mut length = 0;
        while unsafe { *raw.add(length) } != 0 {
            length += 1;
        }
        let path = PathBuf::from(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(raw, length)
        }));
        unsafe { CoTaskMemFree(raw.cast()) };
        Some(path)
    }

    fn migrate_shortcut(folder: &Path, install_root: &Path, log: &mut Vec<String>) {
        // Cheap when already migrated: no file of that name in any casing.
        if !folder.join(SHORTCUT_FILE_NAME).is_file() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(folder) else {
            return;
        };
        let names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        let Some(legacy) = legacy_shortcut_name(names.iter().map(String::as_str)) else {
            return;
        };
        let old = folder.join(legacy);
        let belongs = std::fs::metadata(&old)
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_SHORTCUT_BYTES)
            .and_then(|_| std::fs::read(&old).ok())
            .and_then(|bytes| parse_shell_link(&bytes))
            .is_some_and(|paths| shell_link_points_into(&paths, install_root));
        if !belongs {
            log.push(format!(
                "{} left alone: not this install's shortcut",
                old.display()
            ));
            return;
        }
        // A rename keeps the link itself (target, arguments, icon, working directory).
        match std::fs::rename(&old, folder.join(SHORTCUT_FILE_NAME)) {
            Ok(()) => log.push(format!("{} renamed to {SHORTCUT_FILE_NAME}", old.display())),
            Err(error) => log.push(format!("{} not renamed: {error}", old.display())),
        }
    }

    /// The machine-wide handler the installer wrote; the app rewrites the per-user one itself.
    fn migrate_protocol(install_root: &Path, log: &mut Vec<String>) {
        let Ok(Some(key)) = open_key(HKEY_LOCAL_MACHINE, PROTOCOL_KEY) else {
            return;
        };
        let command = open_key(
            HKEY_LOCAL_MACHINE,
            &format!(r"{PROTOCOL_KEY}\shell\open\command"),
        )
        .ok()
        .flatten()
        .and_then(|command| read_value(&command, ""));
        let description = read_value(&key, "");
        if let Some(value) =
            protocol_description_update(description.as_deref(), command.as_deref(), install_root)
        {
            match write_value(&key, "", value) {
                Ok(()) => log.push(format!("URL protocol description set to {value}")),
                Err(error) => log.push(format!("URL protocol description not set: {error}")),
            }
        }
    }

    pub(super) fn migrate_install(scope: Scope, install_root: &Path, version: &str) -> Vec<String> {
        let mut log = Vec::new();
        migrate_entry(scope, install_root, version, &mut log);
        if scope == Scope::Machine {
            migrate_protocol(install_root, &mut log);
        }
        let folders = match scope {
            Scope::Machine => [&FOLDERID_CommonPrograms, &FOLDERID_PublicDesktop],
            Scope::User => [&FOLDERID_Programs, &FOLDERID_Desktop],
        };
        for folder in folders.into_iter().filter_map(known_folder) {
            migrate_shortcut(&folder, install_root, &mut log);
        }
        log
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const ROOT: &str = r"C:\Program Files\MultiCore";

    fn root() -> PathBuf {
        PathBuf::from(ROOT)
    }

    // Windows paths: `\` separates components only there.
    #[cfg(windows)]
    #[test]
    fn install_root_is_the_parent_of_current_only() {
        assert_eq!(
            install_root_of_current(Path::new(r"C:\Program Files\MultiCore\current")),
            Some(Path::new(r"C:\Program Files\MultiCore"))
        );
        assert_eq!(
            install_root_of_current(Path::new(
                r"C:\Users\me\AppData\Local\Programs\MultiCore\Current"
            )),
            Some(Path::new(r"C:\Users\me\AppData\Local\Programs\MultiCore"))
        );
        assert_eq!(
            install_root_of_current(Path::new(r"D:\portable\MultiCore")),
            None
        );
        assert_eq!(install_root_of_current(Path::new("current")), None);
    }

    #[test]
    fn paths_compare_case_insensitively_beneath_the_root() {
        assert!(path_is_within(
            r"C:\Program Files\MultiCore\current\MultiCore.exe",
            &root()
        ));
        assert!(path_is_within(r"c:\program files\multicore\", &root()));
        assert!(path_is_within(
            r"\\?\C:\Program Files\MultiCore\current",
            &root()
        ));
        assert!(path_is_within(
            "C:/Program Files/MultiCore/current",
            &root()
        ));
        assert!(!path_is_within(
            r"C:\Program Files\MultiCore2\MultiCore.exe",
            &root()
        ));
        assert!(!path_is_within(r"C:\Program Files\MultiCore.exe", &root()));
        assert!(!path_is_within(
            r"D:\Program Files\MultiCore\current",
            &root()
        ));
    }

    const VERSION: &str = "1.1.2";

    fn installed_1_1_1(location: Option<&'static str>) -> UninstallEntry<'static> {
        UninstallEntry {
            install_location: location,
            display_name: Some("MultiCore version 1.1.1"),
            publisher: Some("MultiCore"),
            display_version: Some("1.1.1"),
            major_version: Some(1),
            minor_version: Some(1),
            version_major: Some(1),
            version_minor: Some(1),
        }
    }

    fn text(value: &str) -> EntryValue {
        EntryValue::Text(value.to_owned())
    }

    #[test]
    fn the_uninstall_entry_of_this_install_takes_the_new_name_and_version() {
        let entry = installed_1_1_1(Some(r"C:\Program Files\MultiCore\"));
        assert_eq!(
            uninstall_entry_updates(&entry, &root(), VERSION),
            vec![
                ("DisplayName", text("MULTICORE version 1.1.2")),
                ("Publisher", text("MULTICORE")),
                ("DisplayVersion", text("1.1.2")),
            ]
        );
        let minor_bump = uninstall_entry_updates(&entry, &root(), "1.2.0");
        assert!(minor_bump.contains(&("MinorVersion", EntryValue::Number(2))));
        assert!(minor_bump.contains(&("VersionMinor", EntryValue::Number(2))));
        assert!(!minor_bump.iter().any(|(name, _)| name.contains("Major")));
        assert_eq!(
            uninstall_entry_updates(&entry, &root(), "2.0.0-rc.1")
                .into_iter()
                .filter(|(name, _)| name.contains("Major"))
                .count(),
            2
        );
    }

    #[test]
    fn the_uninstall_entry_follows_every_version_change() {
        // Renamed by an earlier start (or the 1.1.2 installer), then updated in-app again.
        let entry = UninstallEntry {
            install_location: Some(r"C:\Program Files\MultiCore\"),
            display_name: Some("MULTICORE версия 1.1.2"),
            publisher: Some("MULTICORE"),
            display_version: Some("1.1.2"),
            ..UninstallEntry::default()
        };
        assert_eq!(
            uninstall_entry_updates(&entry, &root(), "1.1.3"),
            vec![
                ("DisplayName", text("MULTICORE версия 1.1.3")),
                ("DisplayVersion", text("1.1.3")),
            ]
        );
        // Current: nothing to do.
        assert!(uninstall_entry_updates(&entry, &root(), "1.1.2").is_empty());
        // A name without the version keeps its text; only the version values move.
        let plain = UninstallEntry {
            display_name: Some("MULTICORE"),
            ..entry
        };
        assert_eq!(
            uninstall_entry_updates(&plain, &root(), "1.1.3"),
            vec![("DisplayVersion", text("1.1.3"))]
        );
        // The suffix must be a whole word: "11.1.2" does not end in " 1.1.2".
        let glued = UninstallEntry {
            display_name: Some("MULTICORE 11.1.2"),
            ..entry
        };
        assert_eq!(
            uninstall_entry_updates(&glued, &root(), "1.1.3"),
            vec![("DisplayVersion", text("1.1.3"))]
        );
    }

    #[test]
    fn versions_split_into_major_and_minor() {
        assert_eq!(major_minor("1.1.2"), Some((1, 1)));
        assert_eq!(major_minor("1.12.0-rc.1"), Some((1, 12)));
        assert_eq!(major_minor("2"), None);
        assert_eq!(major_minor("x.1"), None);
    }

    #[test]
    fn another_install_or_an_unknown_location_is_left_alone() {
        for location in [
            None,
            Some(r"C:\Program Files (x86)\MultiCore\"),
            Some(r"C:\Program Files\MultiCore\current"),
            Some(""),
        ] {
            assert!(
                uninstall_entry_updates(&installed_1_1_1(location), &root(), VERSION).is_empty(),
                "{location:?}"
            );
        }
    }

    #[test]
    fn the_service_is_renamed_until_it_carries_the_new_names() {
        assert_eq!(
            service_updates(
                Some("MultiCore"),
                Some("MultiCore: ядра Mihomo и Xray для мгновенного подключения")
            ),
            (Some(SERVICE_DISPLAY_NAME), Some(SERVICE_DESCRIPTION))
        );
        assert_eq!(
            service_updates(None, None),
            (Some(SERVICE_DISPLAY_NAME), None)
        );
        // An English installer's description is not turned back into Russian.
        assert_eq!(
            service_updates(
                Some(SERVICE_DISPLAY_NAME),
                Some("MULTICORE: Mihomo and Xray cores for instant connection")
            ),
            (None, None)
        );
        assert_eq!(
            service_updates(Some(SERVICE_DISPLAY_NAME), Some(SERVICE_DESCRIPTION)),
            (None, None)
        );
    }

    #[test]
    fn the_protocol_description_of_this_install_is_renamed() {
        let command = r#""C:\Program Files\MultiCore\current\MultiCore.exe" "%1""#;
        let legacy = Some("URL:MultiCore Protocol");
        assert_eq!(
            protocol_description_update(legacy, Some(command), &root()),
            Some(PROTOCOL_DESCRIPTION)
        );
        assert_eq!(
            protocol_description_update(Some(PROTOCOL_DESCRIPTION), Some(command), &root()),
            None
        );
        let foreign = r#""D:\Other\MultiCore.exe" "%1""#;
        assert_eq!(
            protocol_description_update(legacy, Some(foreign), &root()),
            None
        );
        assert_eq!(protocol_description_update(legacy, None, &root()), None);
        assert_eq!(
            protocol_description_update(legacy, Some("unquoted.exe %1"), &root()),
            None
        );
    }

    #[test]
    fn only_the_old_spelling_of_the_shortcut_is_picked() {
        assert_eq!(
            legacy_shortcut_name(["Other.lnk", "MultiCore.lnk"]),
            Some("MultiCore.lnk")
        );
        assert_eq!(
            legacy_shortcut_name(["multicore.LNK"]),
            Some("multicore.LNK")
        );
        assert_eq!(legacy_shortcut_name(["MULTICORE.lnk", "x.lnk"]), None);
        assert_eq!(legacy_shortcut_name(["MultiCore.url", "MultiCore"]), None);
    }

    /// A shell link as `IShellLink` writes one: header, a tiny ID list, LinkInfo with the local
    /// base path, then the working directory.
    fn shell_link(target: &str, unicode_target: bool, working_dir: Option<&str>) -> Vec<u8> {
        let mut flags = HAS_LINK_TARGET_ID_LIST | HAS_LINK_INFO | IS_UNICODE;
        if working_dir.is_some() {
            flags |= HAS_WORKING_DIR;
        }
        let mut bytes = Vec::new();
        bytes.extend((SHELL_LINK_HEADER_SIZE as u32).to_le_bytes());
        bytes.extend(SHELL_LINK_CLSID);
        bytes.extend(flags.to_le_bytes());
        bytes.resize(SHELL_LINK_HEADER_SIZE, 0);
        // ID list: one empty terminator item.
        bytes.extend(2_u16.to_le_bytes());
        bytes.extend(0_u16.to_le_bytes());
        // LinkInfo.
        let header_size: u32 = if unicode_target { 0x24 } else { 0x1C };
        let volume_id = [0x10_u8, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0];
        let ansi: Vec<u8> = target
            .chars()
            // Non-ASCII as code-page bytes (cp1251-like), as on a Russian-locale machine.
            .map(|c| if c.is_ascii() { c as u8 } else { 0xC8 })
            .chain([0])
            .collect();
        let volume_offset = header_size;
        let base_offset = volume_offset + volume_id.len() as u32;
        let suffix_offset = base_offset + ansi.len() as u32;
        let unicode_offset = suffix_offset + 1;
        let unicode: Vec<u8> = target
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut info = Vec::new();
        info.extend(0_u32.to_le_bytes()); // size, patched below
        info.extend(header_size.to_le_bytes());
        info.extend(VOLUME_ID_AND_LOCAL_BASE_PATH.to_le_bytes());
        info.extend(volume_offset.to_le_bytes());
        info.extend(base_offset.to_le_bytes());
        info.extend(0_u32.to_le_bytes());
        info.extend(suffix_offset.to_le_bytes());
        if unicode_target {
            info.extend(unicode_offset.to_le_bytes());
            info.extend(0_u32.to_le_bytes());
        }
        info.extend(volume_id);
        info.extend(&ansi);
        info.push(0);
        if unicode_target {
            info.extend(&unicode);
        }
        let size = info.len() as u32;
        info[..4].copy_from_slice(&size.to_le_bytes());
        bytes.extend(info);
        if let Some(directory) = working_dir {
            let units: Vec<u16> = directory.encode_utf16().collect();
            bytes.extend((units.len() as u16).to_le_bytes());
            bytes.extend(units.iter().flat_map(|unit| unit.to_le_bytes()));
        }
        bytes.extend(0_u32.to_le_bytes()); // terminal block
        bytes
    }

    #[test]
    fn shell_link_targets_are_read() {
        let link = shell_link(
            r"C:\Program Files\MultiCore\current\MultiCore.exe",
            false,
            Some(r"C:\Program Files\MultiCore\current"),
        );
        assert_eq!(
            parse_shell_link(&link),
            Some(ShellLinkPaths {
                target_unicode: None,
                target_ansi: Some(br"C:\Program Files\MultiCore\current\MultiCore.exe".to_vec()),
                working_dir: Some(r"C:\Program Files\MultiCore\current".into()),
            })
        );
        let link = shell_link(r"C:\Users\Иван\MultiCore\current\MultiCore.exe", true, None);
        let paths = parse_shell_link(&link).unwrap();
        assert_eq!(
            paths.target_unicode.as_deref(),
            Some(r"C:\Users\Иван\MultiCore\current\MultiCore.exe")
        );
        assert!(shell_link_points_into(
            &paths,
            Path::new(r"C:\Users\Иван\MultiCore")
        ));
    }

    #[test]
    fn a_shortcut_belongs_to_this_install_only_by_its_target() {
        let ours = parse_shell_link(&shell_link(
            r"C:\Program Files\MultiCore\current\MultiCore.exe",
            false,
            Some(r"C:\Program Files\MultiCore\current"),
        ))
        .unwrap();
        assert!(shell_link_points_into(&ours, &root()));
        let elsewhere = parse_shell_link(&shell_link(
            r"D:\Tools\MultiCore\MultiCore.exe",
            false,
            Some(r"C:\Program Files\MultiCore\current"),
        ))
        .unwrap();
        assert!(!shell_link_points_into(&elsewhere, &root()));
        // An ANSI path this check cannot decode falls back to the working directory.
        let user_root = Path::new(r"C:\Users\Иван\AppData\Local\Programs\MultiCore");
        let non_ascii = parse_shell_link(&shell_link(
            r"C:\Users\Иван\AppData\Local\Programs\MultiCore\current\MultiCore.exe",
            false,
            Some(r"C:\Users\Иван\AppData\Local\Programs\MultiCore\current"),
        ))
        .unwrap();
        assert!(shell_link_points_into(&non_ascii, user_root));
        assert!(!shell_link_points_into(&non_ascii, &root()));
    }

    #[test]
    fn malformed_shell_links_are_rejected() {
        let link = shell_link(r"C:\x.exe", true, Some(r"C:\"));
        assert_eq!(parse_shell_link(b"not a link"), None);
        for length in 0..link.len() - 4 {
            // Truncations never panic; most are rejected.
            let _ = parse_shell_link(&link[..length]);
        }
        let mut wrong_clsid = link.clone();
        wrong_clsid[4] ^= 0xFF;
        assert_eq!(parse_shell_link(&wrong_clsid), None);
    }

    #[test]
    fn the_installer_uses_the_same_names() {
        let installer = include_str!("../../../packaging/windows-x64/installer.iss");
        assert!(installer.contains(&format!("#define AppName \"{PRODUCT_NAME}\"")));
        assert!(installer.contains(&format!("#define AppPublisher \"{PRODUCT_NAME}\"")));
        assert!(installer.contains(&format!("DisplayName= \"{SERVICE_DISPLAY_NAME}\"")));
        assert!(installer.contains(&format!("russian.ServiceDescription={SERVICE_DESCRIPTION}")));
        assert!(installer.contains(&format!("ValueData: \"{PROTOCOL_DESCRIPTION}\"")));
        assert!(installer.contains("AppId={{D95AB954-248C-44C9-9E3A-952398802A2D}"));
        assert!(UNINSTALL_KEY.ends_with(r"\{D95AB954-248C-44C9-9E3A-952398802A2D}_is1"));
        assert!(installer.contains(r#"Name: "{autoprograms}\MULTICORE"; "#));
        assert!(SERVICE_DISPLAY_NAME.to_lowercase() != LEGACY_PRODUCT_NAME.to_lowercase());
    }
}
