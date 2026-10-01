use multicore_core::RuntimePaths;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_FILE_OBJECT},
        OWNER_SECURITY_INFORMATION,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle,
    },
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

#[derive(Debug)]
pub(crate) enum ResolveError {
    InvalidGeneration,
    Layout,
    Reparse,
    Escape,
    Missing,
    InvalidPe,
    Io,
    /// A user-side path is owned by someone other than the client (or SYSTEM/Administrators).
    Owner,
}

const SYSTEM_SID: &str = "S-1-5-18";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// Who may own the client's data root and staged generation: the client itself, or SYSTEM /
/// Administrators (who can write there anyway). Anyone else means the client pointed its
/// known folder at another user's files.
pub(crate) fn owner_is_acceptable(owner: &str, client_sid: &str) -> bool {
    owner.eq_ignore_ascii_case(client_sid) || owner == SYSTEM_SID || owner == ADMINISTRATORS_SID
}

/// The owner SID of an open file or directory handle, as `S-1-…` text.
pub(crate) fn handle_owner_sid(file: &File) -> Option<String> {
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    } != 0
    {
        return None;
    }
    let mut text = std::ptr::null_mut();
    let converted = unsafe { ConvertSidToStringSidW(owner, &mut text) } != 0;
    let sid = converted.then(|| {
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) })
    });
    unsafe {
        if converted {
            LocalFree(text.cast());
        }
        LocalFree(descriptor.cast());
    }
    sid
}

pub(crate) struct ResolvedRuntime {
    pub paths: RuntimePaths,
    /// The user's `%LOCALAPPDATA%\MultiCore` directory (already resolved and
    /// trusted by this same function for `runtime/`) - exposed so callers
    /// can place their own per-user files (for example a persistent core
    /// log) alongside `profiles/`, `runtime/`, etc. without a second lookup.
    pub data_root: PathBuf,
    guards: Vec<GuardedPath>,
}

struct GuardedPath {
    path: PathBuf,
    handle: File,
    identity: FileIdentity,
    directory: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    volume: u32,
    index: u64,
}

impl ResolvedRuntime {
    /// Service mode: every guarded directory and file at or below `data_root` (the data root,
    /// `runtime`, the generation directory and its configs) must be owned by `client_sid`, or
    /// by SYSTEM / Administrators. The data root comes from the client's own known folder,
    /// which the client can redirect, so this is what binds the files to the client.
    pub(crate) fn verify_owned_by(
        &self,
        data_root: &Path,
        client_sid: &str,
    ) -> Result<(), ResolveError> {
        self.verify_owned_with(data_root, client_sid, |_, file| handle_owner_sid(file))
    }

    pub(crate) fn verify_owned_with(
        &self,
        data_root: &Path,
        client_sid: &str,
        owner_of: impl Fn(&Path, &File) -> Option<String>,
    ) -> Result<(), ResolveError> {
        let mut checked = 0;
        for guard in self
            .guards
            .iter()
            .filter(|guard| guard.path.starts_with(data_root))
        {
            let owner = owner_of(&guard.path, &guard.handle).ok_or(ResolveError::Owner)?;
            if !owner_is_acceptable(&owner, client_sid) {
                return Err(ResolveError::Owner);
            }
            checked += 1;
        }
        // The data root, runtime, the generation and both configs at least.
        if checked < 5 {
            return Err(ResolveError::Owner);
        }
        Ok(())
    }

    pub(crate) fn verify_unchanged(&self) -> Result<(), ResolveError> {
        for guard in &self.guards {
            let reopened = open_guard(&guard.path, guard.directory)?;
            if reopened.identity != guard.identity {
                return Err(ResolveError::Escape);
            }
            let _ = guard.handle.as_raw_handle();
        }
        Ok(())
    }

    /// Reads a guarded file (matched by file name, e.g. `mihomo.yaml`) through the handle opened
    /// during resolution (reparse points refused, writers excluded by the share mode) instead of
    /// re-opening its path. More than `limit` bytes is `Layout`.
    pub(crate) fn read_guarded(&self, path: &Path, limit: u64) -> Result<Vec<u8>, ResolveError> {
        let guard = self
            .guards
            .iter()
            .rev()
            .find(|guard| !guard.directory && guard.path.file_name() == path.file_name())
            .ok_or(ResolveError::Missing)?;
        let mut handle = &guard.handle;
        handle
            .seek(SeekFrom::Start(0))
            .map_err(|_| ResolveError::Io)?;
        let mut bytes = Vec::new();
        handle
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ResolveError::Io)?;
        if bytes.len() as u64 > limit {
            return Err(ResolveError::Layout);
        }
        Ok(bytes)
    }
}

pub(crate) fn resolve_current(generation: u64) -> Result<ResolvedRuntime, ResolveError> {
    let data = local_app_data()?.join("MultiCore");
    let executable = std::env::current_exe().map_err(|_| ResolveError::Io)?;
    resolve_from(&data, &executable, generation)
}

fn local_app_data() -> Result<PathBuf, ResolveError> {
    let mut raw = std::ptr::null_mut();
    if unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    } < 0
    {
        return Err(ResolveError::Io);
    }
    let mut length = 0;
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    let result = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(result)
}

pub(crate) fn resolve_from(
    data_root: &Path,
    host_executable: &Path,
    generation: u64,
) -> Result<ResolvedRuntime, ResolveError> {
    if generation == 0 {
        return Err(ResolveError::InvalidGeneration);
    }
    let runtime_dir = host_executable.parent().ok_or(ResolveError::Layout)?;
    if runtime_dir.file_name().and_then(|v| v.to_str()) != Some("runtime")
        || host_executable.file_name().and_then(|v| v.to_str()) != Some("multicore-core-host.exe")
    {
        return Err(ResolveError::Layout);
    }
    let package_lexical = runtime_dir.parent().ok_or(ResolveError::Layout)?;
    let cores = package_lexical.join("cores");
    let runtime_root = data_root.join("runtime");
    let generation_dir = runtime_root.join(format!("runtime-{generation:020}"));
    if generation_dir.file_name().and_then(|v| v.to_str())
        != Some(format!("runtime-{generation:020}").as_str())
    {
        return Err(ResolveError::InvalidGeneration);
    }
    let xray = cores.join("xray.exe");
    let mihomo = cores.join("mihomo.exe");
    let xray_config = generation_dir.join("xray.json");
    let mihomo_config = generation_dir.join("mihomo.yaml");
    // The vendored dashboard is optional: absent in builds without one, present as an exact
    // package-root sibling of `cores` otherwise. Its presence is decided once, up front, so a
    // directory that appears only after that check can never be picked up mid-resolution.
    let dashboard = package_lexical.join("dashboard");
    // Any entry counts as present, so a dashboard replaced by a link or junction is rejected
    // by the reparse guard below rather than silently treated as absent (a directory symlink's
    // own metadata is not `is_dir`).
    let dashboard_present = fs::symlink_metadata(&dashboard).is_ok();
    let mut guards = Vec::new();
    for directory in [
        data_root,
        &runtime_root,
        &generation_dir,
        package_lexical,
        runtime_dir,
        &cores,
    ] {
        guard_lexical_chain(directory, true, &mut guards)?;
    }
    if dashboard_present {
        guard_lexical_chain(&dashboard, true, &mut guards)?;
    }
    for file in [
        host_executable,
        &xray,
        &mihomo,
        &xray_config,
        &mihomo_config,
    ] {
        guard_lexical_chain(file, false, &mut guards)?;
    }

    let host = fs::canonicalize(host_executable).map_err(|_| ResolveError::Missing)?;
    let package = fs::canonicalize(package_lexical).map_err(|_| ResolveError::Missing)?;
    let canonical_data = fs::canonicalize(data_root).map_err(|_| ResolveError::Missing)?;
    let canonical_generation =
        fs::canonicalize(&generation_dir).map_err(|_| ResolveError::Missing)?;
    canonical_contained(&package, &host)?;
    canonical_contained(&canonical_data, &canonical_generation)?;
    for binary in [&xray, &mihomo] {
        canonical_contained(
            &package,
            &fs::canonicalize(binary).map_err(|_| ResolveError::Missing)?,
        )?;
    }
    for config in [&xray_config, &mihomo_config] {
        canonical_contained(
            &canonical_generation,
            &fs::canonicalize(config).map_err(|_| ResolveError::Missing)?,
        )?;
    }
    let dashboard_directory = if dashboard_present {
        let canonical_dashboard =
            fs::canonicalize(&dashboard).map_err(|_| ResolveError::Missing)?;
        canonical_contained(&package, &canonical_dashboard)?;
        // `fs::canonicalize` returns Windows' `\\?\`-prefixed extended-length form. That form is
        // kept for the containment check above (it defeats reparse-point tricks the way the rest
        // of this resolver relies on), but Mihomo's own SAFE_PATHS check does a plain string
        // prefix comparison against the *unprefixed* `external-ui` path the daemon writes into
        // the generated config (crates/multicore-daemon/src/main.rs), never normalizing away
        // `\\?\`. Passed through unprefixed, Mihomo rejected every dashboard path as "not a
        // subpath of home directory or SAFE_PATHS" and refused to start at all - confirmed live
        // against the pinned Mihomo binary. Strip it only for the value that leaves this
        // function as SAFE_PATHS.
        Some(strip_verbatim_prefix(canonical_dashboard))
    } else {
        None
    };
    for binary in [&xray, &mihomo] {
        let guard = guards
            .iter_mut()
            .rev()
            .find(|guard| guard.path == *binary)
            .ok_or(ResolveError::Missing)?;
        validate_native_pe(&mut guard.handle)?;
    }
    Ok(ResolvedRuntime {
        data_root: canonical_data.clone(),
        paths: RuntimePaths {
            xray_binary: launchable_program_path(
                fs::canonicalize(xray).map_err(|_| ResolveError::Missing)?,
            ),
            mihomo_binary: launchable_program_path(
                fs::canonicalize(mihomo).map_err(|_| ResolveError::Missing)?,
            ),
            xray_config: fs::canonicalize(xray_config).map_err(|_| ResolveError::Missing)?,
            mihomo_config: fs::canonicalize(mihomo_config).map_err(|_| ResolveError::Missing)?,
            dashboard_directory,
            mihomo_home_directory: None,
        },
        guards,
    })
}

/// Strips Windows' `\\?\` (or `\\?\UNC\`) extended-length prefix that
/// `fs::canonicalize` adds, without altering anything else about the path.
/// Only for values that must byte-for-byte string-match a path written
/// elsewhere by code that never adds this prefix (Mihomo's own SAFE_PATHS
/// comparison); the security-relevant containment check runs on the
/// still-fully-canonical path before this ever applies.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// The path a core is launched from. Mihomo's TUN (sing-tun, `stack: system`)
/// adds its own inbound Windows Firewall rule for its executable path, and
/// that fails silently for a `\\?\`-prefixed path: launched that way no rule
/// was created and the firewall dropped the connections the TUN redirects to
/// Mihomo, so on a machine without a leftover rule most traffic (Telegram,
/// browsers) failed while connected. From the plain path the rule is
/// created. The files stay guarded and were checked against their canonical
/// paths above; the prefix is only kept when the plain form would exceed
/// `MAX_PATH`.
fn launchable_program_path(canonical: PathBuf) -> PathBuf {
    const MAX_PATH: usize = 260;
    let plain = strip_verbatim_prefix(canonical.clone());
    if plain.as_os_str().len() < MAX_PATH {
        plain
    } else {
        canonical
    }
}

/// Runs `body` with SeBackupPrivilege enabled on this thread only.
///
/// The daemon keeps `%LOCALAPPDATA%\MultiCore` owner-only (a protected DACL with a single ACE
/// for the user), so the always-ready service, running as SYSTEM, is refused when it opens the
/// client's data root to resolve a staged generation. With the privilege enabled, the
/// resolver's opens (`FILE_FLAG_BACKUP_SEMANTICS`, read-only) pass the DACL: the Windows
/// counterpart of the Linux unit's `CAP_DAC_READ_SEARCH`. The resolver's own checks (reparse
/// points, containment, ownership by the client) still decide what is accepted.
///
/// `ImpersonateSelf` gives the thread its own copy of the process token and the privilege is
/// enabled only there, so the process token, which the cores inherit, never has it enabled.
/// Handles opened inside `body` stay usable afterwards. A token that does not hold the
/// privilege (a test run by a standard user) runs `body` with its ordinary access.
pub(crate) fn with_backup_privilege<T>(body: impl FnOnce() -> T) -> T {
    let _privilege = BackupPrivilege::enable();
    body()
}

/// SeBackupPrivilege enabled in this thread's impersonation token; dropping it reverts.
struct BackupPrivilege;

impl BackupPrivilege {
    fn enable() -> Option<Self> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID},
            Security::{
                AdjustTokenPrivileges, ImpersonateSelf, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
                SE_BACKUP_NAME, SE_PRIVILEGE_ENABLED, SecurityImpersonation,
                TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
            },
            System::Threading::{GetCurrentThread, OpenThreadToken},
        };
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_BACKUP_NAME, &mut luid) } == 0 {
            return None;
        }
        if unsafe { ImpersonateSelf(SecurityImpersonation) } == 0 {
            return None;
        }
        // From here on, dropping `impersonation` reverts to the process token.
        let impersonation = Self;
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe {
            OpenThreadToken(
                GetCurrentThread(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                1,
                &mut token,
            )
        } == 0
        {
            return None;
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let enabled = unsafe {
            AdjustTokenPrivileges(
                token,
                0,
                &privileges,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        } != 0
            && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED;
        unsafe { CloseHandle(token) };
        enabled.then_some(impersonation)
    }
}

impl Drop for BackupPrivilege {
    fn drop(&mut self) {
        // A thread left impersonating would keep the privilege for whatever it runs next.
        if unsafe { windows_sys::Win32::Security::RevertToSelf() } == 0 {
            std::process::abort();
        }
    }
}

fn canonical_contained(root: &Path, target: &Path) -> Result<(), ResolveError> {
    let root = fs::canonicalize(root).map_err(|_| ResolveError::Missing)?;
    let target = fs::canonicalize(target).map_err(|_| ResolveError::Missing)?;
    if !target.starts_with(&root) {
        return Err(ResolveError::Escape);
    }
    Ok(())
}

fn guard_lexical_chain(
    path: &Path,
    target_directory: bool,
    guards: &mut Vec<GuardedPath>,
) -> Result<(), ResolveError> {
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for (index, ancestor) in ancestors.into_iter().enumerate() {
        if guards.iter().any(|guard| guard.path == ancestor) {
            continue;
        }
        let final_component = ancestor == path;
        let directory = if final_component {
            target_directory
        } else {
            true
        };
        if index == 0 && ancestor.parent().is_none() {
            continue;
        }
        guards.push(open_guard(ancestor, directory)?);
    }
    Ok(())
}

fn open_guard(path: &Path, directory: bool) -> Result<GuardedPath, ResolveError> {
    // FILE_FLAG_BACKUP_SEMANTICS (needed for directories anyway) lets the always-ready service
    // read files as well as directories under `with_backup_privilege`; without the privilege
    // enabled it changes nothing for a file.
    //
    // Nothing guarded may be deleted or renamed (no FILE_SHARE_DELETE) and no guarded file may
    // be written. A directory guard does share writes: for a directory that only admits the
    // target-directory open every rename into it performs. Without it the per-user broker,
    // which keeps these guards for a whole connection, made every atomic replace inside
    // `%LOCALAPPDATA%\MultiCore` (the desktop's preferences) and in every ancestor up to
    // `C:\Users` fail with os error 32, and the service did the same while it read a generation.
    let share = if directory {
        FILE_SHARE_READ | FILE_SHARE_WRITE
    } else {
        FILE_SHARE_READ
    };
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(share)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .map_err(|_| ResolveError::Missing)?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return Err(ResolveError::Io);
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ResolveError::Reparse);
    }
    let actual_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if actual_directory != directory {
        return Err(ResolveError::Layout);
    }
    Ok(GuardedPath {
        path: path.to_path_buf(),
        identity: FileIdentity {
            volume: info.dwVolumeSerialNumber,
            index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        },
        handle,
        directory,
    })
}

/// The cores must be images of this build's architecture (PE32+ AMD64 or PE32 i386).
fn validate_native_pe(file: &mut File) -> Result<(), ResolveError> {
    let arch = multicore_release::WindowsArch::CURRENT;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)
        .map_err(|_| ResolveError::InvalidPe)?;
    if &dos[..2] != b"MZ" {
        return Err(ResolveError::InvalidPe);
    }
    let offset = u32::from_le_bytes(dos[0x3c..0x40].try_into().unwrap()) as u64;
    if !(64..=16 * 1024 * 1024).contains(&offset) {
        return Err(ResolveError::InvalidPe);
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| ResolveError::InvalidPe)?;
    let mut header = [0u8; 26];
    file.read_exact(&mut header)
        .map_err(|_| ResolveError::InvalidPe)?;
    let machine = u16::from_le_bytes([header[4], header[5]]);
    let characteristics = u16::from_le_bytes([header[22], header[23]]);
    let magic = u16::from_le_bytes([header[24], header[25]]);
    if &header[..4] != b"PE\0\0"
        || machine != arch.pe_machine()
        || magic != arch.pe_optional_magic()
        || characteristics & 0x0002 == 0
        || characteristics & 0x2000 != 0
    {
        return Err(ResolveError::InvalidPe);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "multicore-host-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    const NATIVE_MACHINE: u16 = multicore_release::WindowsArch::CURRENT.pe_machine();
    const FOREIGN_MACHINE: u16 = match multicore_release::WindowsArch::CURRENT {
        multicore_release::WindowsArch::X64 => 0x014c,
        multicore_release::WindowsArch::X86 => 0x8664,
    };
    fn pe(path: &Path, machine: u16, dll: bool) {
        let mut bytes = vec![0u8; 0x100];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        let flags = 0x0002u16 | if dll { 0x2000 } else { 0 };
        bytes[0x96..0x98].copy_from_slice(&flags.to_le_bytes());
        let magic = multicore_release::WindowsArch::CURRENT.pe_optional_magic();
        bytes[0x98..0x9a].copy_from_slice(&magic.to_le_bytes());
        fs::write(path, bytes).unwrap();
    }
    fn fixture() -> (TestDir, PathBuf, PathBuf) {
        let root = TestDir::new();
        let package = root.path().join("Program Files").join("MultiCore");
        fs::create_dir_all(package.join("runtime")).unwrap();
        fs::create_dir_all(package.join("cores")).unwrap();
        let host = package.join("runtime/multicore-core-host.exe");
        fs::write(&host, b"host").unwrap();
        pe(&package.join("cores/xray.exe"), NATIVE_MACHINE, false);
        pe(&package.join("cores/mihomo.exe"), NATIVE_MACHINE, false);
        let data = root.path().join("Local/MultiCore");
        fs::create_dir_all(data.join("runtime/runtime-00000000000000000007")).unwrap();
        fs::write(
            data.join("runtime/runtime-00000000000000000007/xray.json"),
            b"{}",
        )
        .unwrap();
        fs::write(
            data.join("runtime/runtime-00000000000000000007/mihomo.yaml"),
            b"tun: {}",
        )
        .unwrap();
        (root, data, host)
    }
    const CLIENT: &str = "S-1-5-21-1-2-3-1001";
    const OTHER: &str = "S-1-5-21-1-2-3-1002";

    /// Replaces the DACL of `path` (and, through inheritance, of everything below it).
    fn set_dacl(path: &Path, sddl: &str) {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Security::{
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION,
        };
        let sddl = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let (mut present, mut defaulted, mut dacl) = (0, 0, std::ptr::null_mut());
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        let mut wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        unsafe { LocalFree(descriptor.cast()) };
        assert_eq!(status, 0, "SetNamedSecurityInfoW({})", path.display());
    }

    /// The daemon keeps `%LOCALAPPDATA%\MultiCore` owner-only (protected DACL, one ACE for the
    /// user), so the always-ready service, running as SYSTEM, is not in the DACL of the data
    /// root it must read. Like the Linux unit's CAP_DAC_READ_SEARCH, the service reads it with
    /// SeBackupPrivilege. Needs a token that holds the privilege (SYSTEM or an elevated
    /// administrator); anywhere else the check cannot be exercised and the test returns.
    #[test]
    fn service_reads_a_data_root_whose_dacl_leaves_out_the_service_account() {
        let Some(already_enabled) = backup_privilege_in_token() else {
            eprintln!("skipped: SeBackupPrivilege is not held (run as SYSTEM or elevated)");
            return;
        };
        let (_r, d, h) = fixture();
        // Everything below inherits: only the (fictitious) client may open the data root.
        set_dacl(&d, &format!("D:P(A;OICI;FA;;;{CLIENT})"));
        let refused = resolve_from(&d, &h, 7);
        let resolved = with_backup_privilege(|| resolve_from(&d, &h, 7));
        let unchanged = resolved
            .as_ref()
            .map(|resolved| with_backup_privilege(|| resolved.verify_unchanged()));
        let config = resolved
            .as_ref()
            .map(|resolved| resolved.read_guarded(&resolved.paths.xray_config, 1024));
        // The fixture owner may always rewrite the DACL, so the directory can be removed.
        set_dacl(&d, "D:P(A;OICI;FA;;;WD)");
        // Some logons (an OpenSSH session of an administrator) have the privilege enabled for
        // the whole process: then even the plain resolution reads through the DACL.
        if !already_enabled {
            assert!(
                matches!(refused, Err(ResolveError::Missing)),
                "without the privilege the data root must stay unreadable"
            );
        }
        assert!(resolved.is_ok(), "{:?}", resolved.err());
        assert!(matches!(unchanged, Ok(Ok(()))), "{unchanged:?}");
        assert!(
            matches!(config, Ok(Ok(ref bytes)) if bytes == b"{}"),
            "{config:?}"
        );
    }

    /// `None`: the process token does not hold SeBackupPrivilege; `Some(enabled)` otherwise.
    fn backup_privilege_in_token() -> Option<bool> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE, LUID},
            Security::{
                GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_BACKUP_NAME,
                TOKEN_PRIVILEGES, TOKEN_QUERY, TokenPrivileges,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        let mut luid = LUID {
            LowPart: 0,
            HighPart: 0,
        };
        if unsafe { LookupPrivilegeValueW(std::ptr::null(), SE_BACKUP_NAME, &mut luid) } == 0 {
            return None;
        }
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return None;
        }
        let mut buffer = vec![0_u64; 1024];
        let mut length = 0;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenPrivileges,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut length,
            )
        } != 0;
        unsafe { CloseHandle(token) };
        if !ok {
            return None;
        }
        let privileges = unsafe { &*buffer.as_ptr().cast::<TOKEN_PRIVILEGES>() };
        let entries: &[LUID_AND_ATTRIBUTES] = unsafe {
            std::slice::from_raw_parts(
                privileges.Privileges.as_ptr(),
                privileges.PrivilegeCount as usize,
            )
        };
        entries
            .iter()
            .find(|entry| {
                entry.Luid.LowPart == luid.LowPart && entry.Luid.HighPart == luid.HighPart
            })
            .map(|entry| entry.Attributes & windows_sys::Win32::Security::SE_PRIVILEGE_ENABLED != 0)
    }

    #[test]
    fn staged_files_of_another_user_are_refused() {
        let (_r, d, h) = fixture();
        let resolved = resolve_from(&d, &h, 7).unwrap();
        assert!(
            resolved
                .verify_owned_with(&d, CLIENT, |_, _| Some(CLIENT.into()))
                .is_ok()
        );
        assert!(
            resolved
                .verify_owned_with(&d, CLIENT, |_, _| Some("S-1-5-18".into()))
                .is_ok(),
            "SYSTEM-owned files are acceptable"
        );
        for foreign in ["runtime-00000000000000000007", "mihomo.yaml", "MultiCore"] {
            let result = resolved.verify_owned_with(&d, CLIENT, |path, _| {
                Some(
                    if path.file_name().and_then(|n| n.to_str()) == Some(foreign) {
                        OTHER.into()
                    } else {
                        CLIENT.into()
                    },
                )
            });
            assert!(matches!(result, Err(ResolveError::Owner)), "{foreign}");
        }
        assert!(matches!(
            resolved.verify_owned_with(&d, CLIENT, |_, _| None),
            Err(ResolveError::Owner)
        ));
    }

    #[test]
    fn real_owners_are_read_from_the_guarded_handles() {
        let (_r, d, h) = fixture();
        let resolved = resolve_from(&d, &h, 7).unwrap();
        let data = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(&d)
            .unwrap();
        let owner = handle_owner_sid(&data).expect("owner of a fresh directory");
        assert!(owner.starts_with("S-1-5-"), "{owner}");
        assert!(resolved.verify_owned_by(&d, &owner).is_ok());
        if !owner_is_acceptable(&owner, OTHER) {
            assert!(matches!(
                resolved.verify_owned_by(&d, OTHER),
                Err(ResolveError::Owner)
            ));
        }
    }

    #[test]
    fn accepts_exact_layout() {
        let (_r, d, h) = fixture();
        let resolved = resolve_from(&d, &h, 7).unwrap();
        assert_eq!(resolved.paths.dashboard_directory, None);
        // The exact `%LOCALAPPDATA%\MultiCore` directory this function
        // already validated for `runtime/`, exposed so callers (the
        // persistent core log) don't need a second lookup.
        assert_eq!(resolved.data_root, fs::canonicalize(&d).unwrap());
    }
    #[test]
    fn guarded_config_reads_go_through_the_resolved_handle() {
        let (_r, d, h) = fixture();
        let resolved = resolve_from(&d, &h, 7).unwrap();
        assert_eq!(
            resolved
                .read_guarded(&resolved.paths.mihomo_config, 1024)
                .unwrap(),
            b"tun: {}"
        );
        assert_eq!(
            resolved
                .read_guarded(&resolved.paths.xray_config, 1024)
                .unwrap(),
            b"{}"
        );
        assert!(matches!(
            resolved.read_guarded(&resolved.paths.mihomo_config, 3),
            Err(ResolveError::Layout)
        ));
        // The guard's share mode keeps the staged file from being swapped while resolved.
        let config = d.join("runtime/runtime-00000000000000000007/mihomo.yaml");
        assert!(fs::rename(&config, config.with_extension("moved")).is_err());
    }
    /// The per-user broker keeps these guards for a whole connection. Held sharing reads only,
    /// every directory guard refused the target-directory open a rename performs, so nothing
    /// could be atomically replaced inside `%LOCALAPPDATA%\MultiCore` (the desktop's
    /// `preferences.json`) or anywhere up to `C:\Users` while connected: os error 32. The
    /// guarded directories themselves must still stay pinned.
    #[test]
    fn renames_into_guarded_directories_succeed_while_the_guards_stay_pinned() {
        let (r, d, h) = fixture();
        let resolved = resolve_from(&d, &h, 7).unwrap();
        let generation = d.join("runtime/runtime-00000000000000000007");
        for directory in [
            r.path().to_path_buf(),
            d.clone(),
            d.join("runtime"),
            generation,
        ] {
            let target = directory.join("preferences.json");
            fs::write(&target, b"old").unwrap();
            let temporary = directory.join(".preferences.tmp");
            fs::write(&temporary, b"new").unwrap();
            fs::rename(&temporary, &target)
                .unwrap_or_else(|error| panic!("rename into {}: {error}", directory.display()));
            assert_eq!(fs::read(&target).unwrap(), b"new");
            fs::remove_file(&target).unwrap();
        }
        assert!(fs::rename(&d, append_suffix(&d, "-moved")).is_err());
        assert!(fs::rename(d.join("runtime"), d.join("runtime-moved")).is_err());
        drop(resolved);
    }
    #[test]
    fn resolves_an_optional_dashboard_sibling_of_cores() {
        let (_r, d, h) = fixture();
        let package = h.parent().unwrap().parent().unwrap().to_path_buf();
        let dashboard = package.join("dashboard");
        fs::create_dir_all(&dashboard).unwrap();
        fs::write(dashboard.join("index.html"), b"<html></html>").unwrap();

        let resolved = resolve_from(&d, &h, 7).unwrap();
        let canonical = fs::canonicalize(&dashboard).unwrap();
        let stripped = resolved.paths.dashboard_directory.clone().unwrap();
        // Must resolve to the same real directory as fs::canonicalize (still fully
        // symlink/reparse-point resistant - see the reparse-point test below) but without the
        // `\\?\` prefix, which is required to byte-for-byte string-match the unprefixed
        // `external-ui` path the daemon writes elsewhere for Mihomo's own SAFE_PATHS comparison.
        assert_eq!(stripped, strip_verbatim_prefix(canonical));
        assert!(!stripped.to_string_lossy().starts_with(r"\\?\"));
    }

    #[test]
    fn strips_only_the_windows_verbatim_prefix() {
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\C:\Users\Example\dashboard")),
            PathBuf::from(r"C:\Users\Example\dashboard")
        );
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"\\?\UNC\server\share\dashboard")),
            PathBuf::from(r"\\server\share\dashboard")
        );
        // A path without the prefix is returned unchanged.
        assert_eq!(
            strip_verbatim_prefix(PathBuf::from(r"C:\Users\Example\dashboard")),
            PathBuf::from(r"C:\Users\Example\dashboard")
        );
    }
    #[test]
    fn cores_launch_from_plain_paths_unless_too_long() {
        assert_eq!(
            launchable_program_path(PathBuf::from(r"\\?\C:\Users\Example\cores\mihomo.exe")),
            PathBuf::from(r"C:\Users\Example\cores\mihomo.exe")
        );
        let long = format!(r"\\?\C:\{}\mihomo.exe", "d".repeat(300));
        assert_eq!(
            launchable_program_path(PathBuf::from(&long)),
            PathBuf::from(&long)
        );
    }

    #[test]
    fn rejects_a_dashboard_directory_replaced_with_a_reparse_point() {
        let (_r, d, h) = fixture();
        let package = h.parent().unwrap().parent().unwrap().to_path_buf();
        let dashboard = package.join("dashboard");
        let outside = d.parent().unwrap().join("outside-dashboard");
        fs::create_dir_all(&outside).unwrap();
        match std::os::windows::fs::symlink_dir(&outside, &dashboard) {
            Ok(()) => assert!(matches!(
                resolve_from(&d, &h, 7),
                Err(ResolveError::Reparse)
            )),
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) => {}
            Err(error) => panic!("could not create dashboard reparse fixture: {error}"),
        }
    }
    #[test]
    fn rejects_zero_missing_and_wrong_pe() {
        let (_r, d, h) = fixture();
        assert!(matches!(
            resolve_from(&d, &h, 0),
            Err(ResolveError::InvalidGeneration)
        ));
        assert!(matches!(
            resolve_from(&d, &h, 8),
            Err(ResolveError::Missing)
        ));
        pe(
            &h.parent().unwrap().parent().unwrap().join("cores/xray.exe"),
            FOREIGN_MACHINE,
            false,
        );
        assert!(matches!(
            resolve_from(&d, &h, 7),
            Err(ResolveError::InvalidPe)
        ));
    }
    #[test]
    fn rejects_dll_core_and_wrong_host_layout() {
        let (_r, d, h) = fixture();
        pe(
            &h.parent().unwrap().parent().unwrap().join("cores/xray.exe"),
            NATIVE_MACHINE,
            true,
        );
        assert!(matches!(
            resolve_from(&d, &h, 7),
            Err(ResolveError::InvalidPe)
        ));
        assert!(matches!(
            resolve_from(&d, &h.with_file_name("other.exe"), 7),
            Err(ResolveError::Missing | ResolveError::Layout)
        ));
    }
    #[test]
    fn rejects_escape_and_reparse_components() {
        let (root, data, host) = fixture();
        assert!(matches!(
            canonical_contained(&data, &host),
            Err(ResolveError::Escape)
        ));
        let target = root.path().join("real-directory");
        let link = root.path().join("linked-directory");
        fs::create_dir(&target).unwrap();
        match std::os::windows::fs::symlink_dir(&target, &link) {
            Ok(()) => assert!(matches!(
                open_guard(&link, true),
                Err(ResolveError::Reparse)
            )),
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) => {}
            Err(error) => panic!("could not create reparse test fixture: {error}"),
        }
    }

    fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn replace_with_symlink(path: &Path, directory: bool) -> Result<(), std::io::Error> {
        let real = append_suffix(path, ".real");
        fs::rename(path, &real)?;
        let result = if directory {
            std::os::windows::fs::symlink_dir(&real, path)
        } else {
            std::os::windows::fs::symlink_file(&real, path)
        };
        if result.is_err() {
            let _ = fs::rename(&real, path);
        }
        result
    }

    #[test]
    fn full_resolver_rejects_reparse_at_every_trusted_layer() {
        for target in ["data", "runtime", "generation", "cores", "binary", "config"] {
            let (_root, data, host) = fixture();
            let package = host.parent().unwrap().parent().unwrap();
            let (path, directory) = match target {
                "data" => (data.clone(), true),
                "runtime" => (data.join("runtime"), true),
                "generation" => (data.join("runtime/runtime-00000000000000000007"), true),
                "cores" => (package.join("cores"), true),
                "binary" => (package.join("cores/xray.exe"), false),
                "config" => (
                    data.join("runtime/runtime-00000000000000000007/xray.json"),
                    false,
                ),
                _ => unreachable!(),
            };
            match replace_with_symlink(&path, directory) {
                Ok(()) => assert!(
                    matches!(resolve_from(&data, &host, 7), Err(ResolveError::Reparse)),
                    "accepted reparse at {target}"
                ),
                Err(error)
                    if error.kind() == std::io::ErrorKind::PermissionDenied
                        || error.raw_os_error() == Some(1314) => {}
                Err(error) => panic!("could not create {target} reparse fixture: {error}"),
            }
        }
    }
}
