//! The service's own admin/root-only directory: sanitised generations, provider caches,
//! Mihomo's home, core logs, and update downloads.
//!
//! Layout under [`service_directory`]:
//! - `runtime-<generation:020>/` — the sanitised `mihomo.yaml` and `xray.json` of the running
//!   generation (older generations are deleted when a new one is prepared);
//! - `providers/` — Mihomo provider caches, one file per provider name;
//! - `logs/latest-core.log` — the cores' persistent log;
//! - everything else Mihomo keeps in its home (`cache.db`, geodata).

use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) fn fresh_generation_directory(
    service_directory: &Path,
    generation: u64,
) -> io::Result<PathBuf> {
    for entry in fs::read_dir(service_directory)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("runtime-"))
        {
            // `remove_dir_all` removes symlinks/junctions themselves, never their targets.
            fs::remove_dir_all(entry.path())?;
        }
    }
    let directory = service_directory.join(format!("runtime-{generation:020}"));
    fs::create_dir(&directory)?;
    Ok(directory)
}

#[cfg(unix)]
pub(crate) fn service_directory() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/var/lib/multicore"))
}

/// Makes sure `path` is a real directory owned by the service's own user with mode 0700.
#[cfg(unix)]
pub(crate) fn ensure_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(
            "service directory is not a real directory",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::other("service directory has a foreign owner"));
    }
    if metadata.mode() & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Protected DACL: full control for SYSTEM and Administrators, inherited by children; nothing
/// for users and no inheritance from `C:\ProgramData` (where Users may create folders).
#[cfg(windows)]
pub(crate) const PRIVATE_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

#[cfg(windows)]
pub(crate) fn owner_is_trusted(sid: &str) -> bool {
    matches!(sid, "S-1-5-18" | "S-1-5-32-544")
}

/// `%ProgramData%\MultiCore\service`.
#[cfg(windows)]
pub(crate) fn service_directory() -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    };
    let mut raw = std::ptr::null_mut();
    if unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramData,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut raw,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0;
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    let root = PathBuf::from(std::ffi::OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    unsafe { CoTaskMemFree(raw.cast()) };
    Ok(root.join("MultiCore").join("service"))
}

/// Makes `path` and its parent real directories owned by SYSTEM/Administrators with
/// [`PRIVATE_SDDL`]. A reparse point (or non-directory) at either level is an error and is
/// checked for both levels before anything is moved or created. A directory someone else
/// pre-created is moved aside, never reused.
#[cfg(windows)]
pub(crate) fn ensure_private_directory(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("service directory has no parent"))?;
    for directory in [parent, path] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || !metadata.is_dir()
                {
                    return Err(io::Error::other(
                        "service directory is a reparse point or not a directory",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for directory in [parent, path] {
        match fs::symlink_metadata(directory) {
            Ok(_) => {
                if !owner_is_trusted(&windows_acl::owner_sid(directory)?) {
                    let aside =
                        directory.with_extension(format!("untrusted-{}", std::process::id()));
                    fs::rename(directory, aside)?;
                    windows_acl::create_private(directory)?;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                windows_acl::create_private(directory)?;
            }
            Err(error) => return Err(error),
        }
        windows_acl::apply_private(directory)?;
    }
    Ok(())
}

#[cfg(windows)]
mod windows_acl {
    use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt, path::Path, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, LocalFree},
        Security::{
            ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::CreateDirectoryW,
    };

    fn wide(path: &Path) -> Vec<u16> {
        OsStr::new(path).encode_wide().chain(Some(0)).collect()
    }

    struct Descriptor(PSECURITY_DESCRIPTOR);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe { LocalFree(self.0.cast()) };
        }
    }

    fn private_descriptor() -> io::Result<Descriptor> {
        let sddl = OsStr::new(super::PRIVATE_SDDL)
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Descriptor(descriptor))
    }

    pub(super) fn create_private(path: &Path) -> io::Result<()> {
        let descriptor = private_descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        if unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn apply_private(path: &Path) -> io::Result<()> {
        let descriptor = private_descriptor()?;
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = ptr::null_mut();
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null(),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    pub(super) fn owner_sid(path: &Path) -> io::Result<String> {
        let mut owner: PSID = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let _descriptor = Descriptor(descriptor);
        let mut text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(owner, &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        while unsafe { *text.add(length) } != 0 {
            length += 1;
        }
        let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length) });
        unsafe { LocalFree(text.cast()) };
        Ok(sid)
    }
}
