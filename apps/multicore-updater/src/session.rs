//! Service mode (the updater runs as SYSTEM): start the MultiCore service again, run the
//! desktop in the logged-on user's session, and touch the user's files only as that user.

use std::{ffi::OsStr, io, os::windows::ffi::OsStrExt, path::Path, ptr, time::Duration};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_NOT_ACTIVE, GetLastError, HANDLE,
        WAIT_OBJECT_0,
    },
    Security::{ImpersonateLoggedOnUser, RevertToSelf},
    System::{
        Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
        RemoteDesktop::WTSQueryUserToken,
        Services::{
            CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
            SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_START,
            SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED, StartServiceW,
        },
        Threading::{
            CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
            WaitForSingleObject,
        },
    },
};

use crate::LaunchedClient;

const SERVICE_NAME: &str = "MultiCore";

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

pub(crate) struct UserToken(HANDLE);

impl Drop for UserToken {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// The primary token of the user logged on to `session` (requires SYSTEM).
pub(crate) fn user_token(session: u32) -> io::Result<UserToken> {
    let mut token = ptr::null_mut();
    if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UserToken(token))
}

/// Runs `work` on this thread while impersonating the user.
pub(crate) fn impersonating<T>(token: &UserToken, work: impl FnOnce() -> T) -> io::Result<T> {
    /// Reverts on drop, so a panic in `work` never unwinds as the user.
    struct Revert;
    impl Drop for Revert {
        fn drop(&mut self) {
            if unsafe { RevertToSelf() } == 0 {
                // Continuing as the user would be worse than stopping.
                std::process::abort();
            }
        }
    }

    if unsafe { ImpersonateLoggedOnUser(token.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _revert = Revert;
    Ok(work())
}

pub(crate) struct SessionClient(HANDLE);

impl Drop for SessionClient {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

impl LaunchedClient for SessionClient {
    fn has_exited(&mut self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

/// Starts `executable` as the user on the interactive desktop (`winsta0\default`) of the
/// user's session, with the user's own environment.
pub(crate) fn launch(
    token: &UserToken,
    executable: &Path,
    directory: &Path,
) -> io::Result<SessionClient> {
    let mut environment = ptr::null_mut();
    if unsafe { CreateEnvironmentBlock(&mut environment, token.0, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let application = wide(executable.as_os_str());
    let mut command_line = wide(OsStr::new(&format!("\"{}\"", executable.display())));
    let working = wide(directory.as_os_str());
    let mut desktop = wide(OsStr::new("winsta0\\default"));
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    startup.lpDesktop = desktop.as_mut_ptr();
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessAsUserW(
            token.0,
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_UNICODE_ENVIRONMENT,
            environment,
            working.as_ptr(),
            &startup,
            &mut process,
        )
    };
    let error = io::Error::last_os_error();
    unsafe { DestroyEnvironmentBlock(environment) };
    if created == 0 {
        return Err(error);
    }
    unsafe { CloseHandle(process.hThread) };
    Ok(SessionClient(process.hProcess))
}

/// Starts the MultiCore service; "already running" counts as success.
pub(crate) fn start_service() -> io::Result<()> {
    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(io::Error::last_os_error());
    }
    let name = wide(OsStr::new(SERVICE_NAME));
    let service = unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_START) };
    if service.is_null() {
        let error = io::Error::last_os_error();
        unsafe { CloseServiceHandle(manager) };
        return Err(error);
    }
    let started = unsafe { StartServiceW(service, 0, ptr::null()) };
    let error = unsafe { GetLastError() };
    unsafe {
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
    }
    if started == 0 && error != ERROR_SERVICE_ALREADY_RUNNING {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(())
}

/// Stops the MultiCore service and waits (up to `timeout`) until SCM reports it stopped.
/// "Not running" counts as success.
pub(crate) fn stop_service(timeout: Duration) -> io::Result<()> {
    let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(io::Error::last_os_error());
    }
    let name = wide(OsStr::new(SERVICE_NAME));
    let service =
        unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_STOP | SERVICE_QUERY_STATUS) };
    if service.is_null() {
        let error = io::Error::last_os_error();
        unsafe { CloseServiceHandle(manager) };
        return Err(error);
    }
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    let result = if unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) } == 0
        && unsafe { GetLastError() } != ERROR_SERVICE_NOT_ACTIVE
    {
        Err(io::Error::last_os_error())
    } else {
        let poll = Duration::from_millis(250);
        let mut waited = Duration::ZERO;
        loop {
            if unsafe { QueryServiceStatus(service, &mut status) } == 0 {
                break Err(io::Error::last_os_error());
            }
            if status.dwCurrentState == SERVICE_STOPPED {
                break Ok(());
            }
            if waited >= timeout {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "MultiCore service did not stop in time",
                ));
            }
            std::thread::sleep(poll);
            waited += poll;
        }
    };
    unsafe {
        CloseServiceHandle(service);
        CloseServiceHandle(manager);
    }
    result
}
