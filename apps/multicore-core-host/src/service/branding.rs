//! Service mode: an in-app update replaces only `<install>\current`, so the running service
//! renames what the 1.1.1 installer registered as "MultiCore" (its own display name and
//! description, the Apps & Features entry, the all-users shortcuts). Best effort: it runs on
//! its own thread, logs what it did and never affects the service.

use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use multicore_release::brand::{self, Scope};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW, SC_HANDLE,
    SC_MANAGER_CONNECT, SERVICE_CHANGE_CONFIG, SERVICE_CONFIG_DESCRIPTION, SERVICE_DESCRIPTIONW,
    SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
};

use super::service_log;
use super::windows_scm::SERVICE_NAME;

const MAX_CONFIG_BYTES: u32 = 64 * 1024;

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

struct Handle(SC_HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Reads a NUL-terminated UTF-16 string; `None` for a null pointer or invalid text.
unsafe fn read_wide(pointer: *const u16) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    let mut length = 0;
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    String::from_utf16(unsafe { std::slice::from_raw_parts(pointer, length) }).ok()
}

/// Queries into a u64-aligned buffer sized by the first call.
fn query(mut call: impl FnMut(*mut u8, u32, &mut u32) -> i32) -> Option<Vec<u64>> {
    let mut needed = 0_u32;
    call(null_mut(), 0, &mut needed);
    if needed == 0 || needed > MAX_CONFIG_BYTES {
        return None;
    }
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    let size = (buffer.len() * 8) as u32;
    (call(buffer.as_mut_ptr().cast(), size, &mut needed) != 0).then_some(buffer)
}

fn rename_service(log: &mut Vec<String>) {
    let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        log.push(format!(
            "SCM not opened: {}",
            std::io::Error::last_os_error()
        ));
        return;
    }
    let manager = Handle(manager);
    let name = wide(SERVICE_NAME);
    let service = unsafe {
        OpenServiceW(
            manager.0,
            name.as_ptr(),
            SERVICE_QUERY_CONFIG | SERVICE_CHANGE_CONFIG,
        )
    };
    if service.is_null() {
        log.push(format!(
            "service not opened: {}",
            std::io::Error::last_os_error()
        ));
        return;
    }
    let service = Handle(service);
    let display_name = query(|buffer, size, needed| unsafe {
        QueryServiceConfigW(
            service.0,
            buffer.cast::<QUERY_SERVICE_CONFIGW>(),
            size,
            needed,
        )
    })
    .and_then(|buffer| unsafe {
        read_wide((*buffer.as_ptr().cast::<QUERY_SERVICE_CONFIGW>()).lpDisplayName)
    });
    let description = query(|buffer, size, needed| unsafe {
        QueryServiceConfig2W(service.0, SERVICE_CONFIG_DESCRIPTION, buffer, size, needed)
    })
    .and_then(|buffer| unsafe {
        read_wide((*buffer.as_ptr().cast::<SERVICE_DESCRIPTIONW>()).lpDescription)
    });
    let (new_display_name, new_description) =
        brand::service_updates(display_name.as_deref(), description.as_deref());
    if let Some(value) = new_display_name {
        let value_wide = wide(value);
        let changed = unsafe {
            ChangeServiceConfigW(
                service.0,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                SERVICE_NO_CHANGE,
                null(),
                null(),
                null_mut(),
                null(),
                null(),
                null(),
                value_wide.as_ptr(),
            )
        };
        log.push(if changed != 0 {
            format!("service display name set to {value}")
        } else {
            format!(
                "service display name not set: {}",
                std::io::Error::last_os_error()
            )
        });
    }
    if let Some(value) = new_description {
        let mut value_wide = wide(value);
        let info = SERVICE_DESCRIPTIONW {
            lpDescription: value_wide.as_mut_ptr(),
        };
        let changed = unsafe {
            ChangeServiceConfig2W(
                service.0,
                SERVICE_CONFIG_DESCRIPTION,
                (&raw const info).cast(),
            )
        };
        log.push(if changed != 0 {
            "service description updated".to_owned()
        } else {
            format!(
                "service description not set: {}",
                std::io::Error::last_os_error()
            )
        });
    }
}

/// Renames the service registration and this install's entry and shortcuts.
fn migrate(package_root: &Path) -> Vec<String> {
    let mut log = Vec::new();
    rename_service(&mut log);
    match brand::install_root_of_current(package_root) {
        Some(root) => log.extend(brand::migrate_install(
            Scope::Machine,
            root,
            env!("MULTICORE_APP_VERSION"),
        )),
        None => log.push(format!(
            "{} is not an installed copy; shortcuts left alone",
            package_root.display()
        )),
    }
    log
}

/// Starts the migration on its own thread; failures only reach `logs/service.log`.
pub(crate) fn spawn(service_directory: PathBuf, package_root: PathBuf) {
    // Not spawned: nothing else to do, the next start tries again.
    let _ = std::thread::Builder::new()
        .name("multicore-branding".into())
        .spawn(move || {
            for line in migrate(&package_root) {
                service_log::append(&service_directory, &format!("branding: {line}"));
            }
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_service_renames_itself_only_once_it_reports_running() {
        let source = include_str!("windows_scm.rs");
        let running = source
            .find("report(SERVICE_RUNNING, NO_ERROR, 0);")
            .expect("running report");
        let branding = source
            .find("super::branding::spawn(")
            .expect("the service renames the 1.1.1 registration on start");
        assert!(running < branding, "renaming must never delay the start");
    }
}
