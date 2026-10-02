//! The privileged half of «Устранить неполадки» (Windows).
//!
//! Every [`PrivilegedFix`] maps to one fixed operation: a Service Control Manager call on an
//! allowlisted service name, a device removal by MULTICORE's own fixed instance ID, or a
//! system tool from `System32` with a fixed argument vector. Nothing here takes a path, a
//! service name or a command from the client. Two callers use it: the service
//! (`ServiceCommand::Troubleshoot`) and the UAC-elevated helper
//! (`multicore-core-host --troubleshoot <token>...`, see [`helper_main`]).

use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};

use multicore_core::troubleshoot::{
    DpiService, FixOutcome, PrivilegedFix, encode_helper_exit, execution_order,
    parse_helper_arguments,
};
use multicore_release::brand::{SERVICE_DESCRIPTION, SERVICE_DISPLAY_NAME};
use windows_sys::Win32::{
    Foundation::{
        ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
        ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE, GetLastError,
    },
    System::{
        Com::CoTaskMemFree,
        Services::{
            ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
            CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
            SC_ACTION, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
            SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
            SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW,
            SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
            SERVICE_START, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
            SERVICE_WIN32_OWN_PROCESS, StartServiceW,
        },
        SystemInformation::GetSystemDirectoryW,
        Threading::CREATE_NO_WINDOW,
    },
    UI::Shell::{
        FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    },
};

use crate::wintun_cleanup::{AdapterRemoval, remove_tun_adapter};

/// `DELETE` standard access right (winnt.h).
const DELETE: u32 = 0x0001_0000;
/// The service this product registers (`installer.iss`, `windows_scm::SERVICE_NAME`).
const SERVICE_NAME: &str = "MultiCore";
const SYSTEM_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const SERVICE_START_TIMEOUT: Duration = Duration::from_secs(30);

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

struct ScHandle(SC_HANDLE);
impl Drop for ScHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Runs `fixes` in [`execution_order`] and reports one outcome per fix in request order.
/// `log` receives one line per fix (the service writes it to `service.log`).
pub(crate) fn run_batch(fixes: &[PrivilegedFix], log: &mut dyn FnMut(&str)) -> Vec<FixOutcome> {
    let mut outcomes = vec![FixOutcome::Failed; fixes.len()];
    for index in execution_order(fixes) {
        let outcome = run_fix(fixes[index]);
        log(&format!(
            "troubleshoot: {} -> {outcome:?}",
            fixes[index].token()
        ));
        outcomes[index] = outcome;
    }
    outcomes
}

pub(crate) fn run_fix(fix: PrivilegedFix) -> FixOutcome {
    match fix {
        PrivilegedFix::WinsockReset => system_tool("netsh.exe", &["winsock", "reset"]),
        PrivilegedFix::WinhttpResetProxy => {
            system_tool("netsh.exe", &["winhttp", "reset", "proxy"])
        }
        PrivilegedFix::ResyncTime => resync_time(),
        PrivilegedFix::RemoveDpiService { service } => remove_dpi_service(service),
        PrivilegedFix::ReinstallWintun => match remove_tun_adapter() {
            AdapterRemoval::NothingToDo => FixOutcome::NothingToDo,
            AdapterRemoval::Removed => FixOutcome::Done,
            AdapterRemoval::Failed => FixOutcome::Failed,
        },
        PrivilegedFix::ReinstallService => reinstall_service(),
    }
}

/// `multicore-core-host --troubleshoot <token>...`, started elevated by the daemon. Returns
/// the process exit code: the outcomes encoded by [`encode_helper_exit`], or `2` when the
/// arguments are not exactly a valid batch (nothing ran).
pub(crate) fn helper_main(arguments: &[OsString]) -> u32 {
    let Some(arguments) = arguments
        .iter()
        .map(|argument| argument.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    else {
        return 2;
    };
    let Some(fixes) = parse_helper_arguments(&arguments) else {
        return 2;
    };
    let service_directory = crate::service::state_dir::service_directory().ok();
    let outcomes = run_batch(&fixes, &mut |line| {
        // Only into an existing (installer-created, admin-only) service directory.
        if let Some(directory) = service_directory.as_deref().filter(|path| path.is_dir()) {
            crate::service::service_log::append(directory, &format!("helper {line}"));
        }
    });
    encode_helper_exit(&outcomes)
}

/// Whether `arguments` select the elevated troubleshooting helper.
pub(crate) fn wants_helper(arguments: &[OsString]) -> bool {
    arguments
        .first()
        .is_some_and(|flag| flag == multicore_core::troubleshoot::HELPER_FLAG)
}

fn system_directory() -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; 512];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

/// Runs `System32\<tool>` with `arguments`, no window, no inherited streams, bounded in time.
fn system_tool(tool: &str, arguments: &[&str]) -> FixOutcome {
    let Ok(directory) = system_directory() else {
        return FixOutcome::Failed;
    };
    let Ok(mut child) = Command::new(directory.join(tool))
        .args(arguments)
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    else {
        return FixOutcome::Failed;
    };
    let deadline = Instant::now() + SYSTEM_TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return FixOutcome::Done,
            Ok(Some(_)) | Err(_) => return FixOutcome::Failed,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return FixOutcome::Failed;
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn open_manager(access: u32) -> Option<ScHandle> {
    let manager = unsafe { OpenSCManagerW(null(), null(), access) };
    (!manager.is_null()).then(|| ScHandle(manager))
}

enum Opened {
    Service(ScHandle),
    Missing,
    Failed,
}

fn open_service(manager: &ScHandle, name: &str, access: u32) -> Opened {
    let name = wide(name);
    let service = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if !service.is_null() {
        return Opened::Service(ScHandle(service));
    }
    if unsafe { GetLastError() } == ERROR_SERVICE_DOES_NOT_EXIST {
        Opened::Missing
    } else {
        Opened::Failed
    }
}

fn current_state(service: &ScHandle) -> Option<u32> {
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    (unsafe { QueryServiceStatus(service.0, &mut status) } != 0).then_some(status.dwCurrentState)
}

fn wait_for_state(service: &ScHandle, state: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if current_state(service) == Some(state) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// Asks `service` to stop and waits for it; "not running" counts as stopped.
fn stop_service(service: &ScHandle) -> bool {
    if current_state(service) == Some(SERVICE_STOPPED) {
        return true;
    }
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0
        && unsafe { GetLastError() } != ERROR_SERVICE_NOT_ACTIVE
    {
        return false;
    }
    wait_for_state(service, SERVICE_STOPPED, SERVICE_STOP_TIMEOUT)
}

/// Stops and deletes one allowlisted packet-filter service. A driver that cannot stop yet is
/// still marked for deletion and goes away with the next reboot.
fn remove_dpi_service(service: DpiService) -> FixOutcome {
    let Some(manager) = open_manager(SC_MANAGER_CONNECT) else {
        return FixOutcome::Failed;
    };
    let handle = match open_service(
        &manager,
        service.service_name(),
        SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
    ) {
        Opened::Service(handle) => handle,
        Opened::Missing => return FixOutcome::NothingToDo,
        Opened::Failed => return FixOutcome::Failed,
    };
    let _ = stop_service(&handle);
    if unsafe { DeleteService(handle.0) } != 0
        || unsafe { GetLastError() } == ERROR_SERVICE_MARKED_FOR_DELETE
    {
        FixOutcome::Done
    } else {
        FixOutcome::Failed
    }
}

/// Starts Windows Time when it is stopped (never changes its start type), then forces a
/// resync. A just-started service may not have a time sample yet, so one retry follows.
fn resync_time() -> FixOutcome {
    if let Some(manager) = open_manager(SC_MANAGER_CONNECT)
        && let Opened::Service(time) =
            open_service(&manager, "W32Time", SERVICE_START | SERVICE_QUERY_STATUS)
        && current_state(&time) != Some(SERVICE_RUNNING)
    {
        if unsafe { StartServiceW(time.0, 0, null()) } == 0
            && unsafe { GetLastError() } != ERROR_SERVICE_ALREADY_RUNNING
        {
            return FixOutcome::Failed;
        }
        let _ = wait_for_state(&time, SERVICE_RUNNING, Duration::from_secs(10));
    }
    match system_tool("w32tm.exe", &["/resync", "/force"]) {
        FixOutcome::Done => FixOutcome::Done,
        _ => {
            thread::sleep(Duration::from_secs(3));
            system_tool("w32tm.exe", &["/resync", "/force"])
        }
    }
}

fn known_folder(folder: &windows_sys::core::GUID) -> Option<PathBuf> {
    let mut raw = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT as u32, null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
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

/// Case-insensitive Win32 form without the `\\?\` prefix, for comparisons only.
fn normalized(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .trim_end_matches('\\')
        .to_lowercase()
}

/// The service binary may only be registered from the machine-wide install the installer uses
/// (`<Program Files>\MultiCore\current\runtime\multicore-core-host.exe`): a LocalSystem service
/// pointing into a user-writable folder would hand SYSTEM to any process of that user.
pub(crate) fn is_machine_install_host(executable: &Path, program_files: &[PathBuf]) -> bool {
    let executable = normalized(executable);
    if executable
        .split('\\')
        .any(|part| part == ".." || part == ".")
    {
        return false;
    }
    program_files.iter().any(|root| {
        executable
            == format!(
                r"{}\multicore\current\runtime\multicore-core-host.exe",
                normalized(root)
            )
    })
}

/// The service's command line, exactly as `installer.iss` registers it.
pub(crate) fn service_command_line(executable: &Path) -> String {
    let text = executable.to_string_lossy();
    format!(
        "\"{}\" --service",
        text.strip_prefix(r"\\?\").unwrap_or(&text)
    )
}

/// Stops the `MultiCore` service and registers it again exactly as the installer does
/// (LocalSystem, automatic start, display name, description, restart-on-failure), from this
/// very binary, then starts it. An existing registration is overwritten in place rather than
/// deleted and created again: a delete only completes once every open handle to the service
/// is closed, and a create in between fails until a reboot, which would leave the machine with
/// no service at all.
fn reinstall_service() -> FixOutcome {
    let Ok(executable) = std::env::current_exe().and_then(std::fs::canonicalize) else {
        return FixOutcome::Failed;
    };
    let roots = [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86]
        .into_iter()
        .filter_map(known_folder)
        .collect::<Vec<_>>();
    if !is_machine_install_host(&executable, &roots) {
        return FixOutcome::Failed;
    }
    let Some(manager) = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE) else {
        return FixOutcome::Failed;
    };
    let name = wide(SERVICE_NAME);
    let display = wide(SERVICE_DISPLAY_NAME);
    let command = wide(service_command_line(&executable));
    let local_system = wide("LocalSystem");
    // Also the empty load-order group: none.
    let empty_password = wide("");
    // REG_MULTI_SZ with no entries: no dependencies.
    let no_dependencies = [0_u16, 0];
    let service = match open_service(&manager, SERVICE_NAME, SERVICE_ALL_ACCESS) {
        Opened::Service(service) => {
            if !stop_service(&service) {
                return FixOutcome::Failed;
            }
            if unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command.as_ptr(),
                    empty_password.as_ptr(),
                    null_mut(),
                    no_dependencies.as_ptr(),
                    local_system.as_ptr(),
                    empty_password.as_ptr(),
                    display.as_ptr(),
                )
            } == 0
            {
                return FixOutcome::Failed;
            }
            service
        }
        Opened::Missing => {
            let created = unsafe {
                CreateServiceW(
                    manager.0,
                    name.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command.as_ptr(),
                    null(),
                    null_mut(),
                    null(),
                    null(),
                    null(),
                )
            };
            if created.is_null() {
                return FixOutcome::Failed;
            }
            ScHandle(created)
        }
        Opened::Failed => return FixOutcome::Failed,
    };
    let mut description = wide(SERVICE_DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: description.as_mut_ptr(),
    };
    unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            (&raw const info).cast(),
        )
    };
    // `sc failure MultiCore reset= 86400 actions= restart/5000/restart/5000`.
    let mut actions = [
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5_000,
        },
        SC_ACTION {
            Type: SC_ACTION_RESTART,
            Delay: 5_000,
        },
    ];
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: null_mut(),
        lpCommand: null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    if unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&raw const failure).cast(),
        )
    } == 0
    {
        return FixOutcome::Failed;
    }
    if unsafe { StartServiceW(service.0, 0, null()) } == 0
        && unsafe { GetLastError() } != ERROR_SERVICE_ALREADY_RUNNING
    {
        return FixOutcome::Failed;
    }
    if wait_for_state(&service, SERVICE_RUNNING, SERVICE_START_TIMEOUT) {
        FixOutcome::Done
    } else {
        FixOutcome::Failed
    }
}
