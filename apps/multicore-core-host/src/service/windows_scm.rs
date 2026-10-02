//! Service Control Manager glue: `MultiCore`, own process, runs as LocalSystem.

use std::{
    ffi::{OsStr, c_void},
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
    thread,
};

use windows_sys::{
    Win32::{
        Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR, WAIT_TIMEOUT},
        System::{
            Services::{
                RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
                SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
                SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE,
                SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
                SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
            },
            Threading::{CreateEventW, SetEvent, WaitForSingleObject},
        },
    },
    core::PWSTR,
};

use super::core_loop::{ServiceLoop, Troubleshooter};
use super::cores::ProductionCores;
use super::state_dir;
use super::update::ProductionUpdater;
use super::windows_server::{SERVICE_PIPE_NAME, production_verifier, serve};

pub(crate) const SERVICE_NAME: &str = "MultiCore";
const TICK_MILLISECONDS: u32 = 2_000;
const START_WAIT_HINT_MILLISECONDS: u32 = 10_000;
const STOP_WAIT_HINT_MILLISECONDS: u32 = 15_000;

static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
static CHECKPOINT: AtomicU32 = AtomicU32::new(0);
static STOP_EVENT: OnceLock<Arc<OwnedHandle>> = OnceLock::new();

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

/// Controls the SCM may send in `state`: stop and shutdown while running, nothing while a
/// start or stop is still pending.
pub(crate) fn accepted_controls(state: u32) -> u32 {
    if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
    } else {
        0
    }
}

fn report(state: u32, exit_code: u32, wait_hint: u32) {
    let handle = STATUS_HANDLE.load(Ordering::Acquire) as SERVICE_STATUS_HANDLE;
    if handle.is_null() {
        return;
    }
    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepted_controls(state),
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: if pending {
            CHECKPOINT.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            0
        },
        dwWaitHint: wait_hint,
    };
    unsafe { SetServiceStatus(handle, &status) };
}

/// Hands the process to the SCM dispatcher; returns when the service has stopped. Fails when
/// the process was not started by the SCM.
pub(crate) fn run() -> Result<(), ()> {
    let name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_ptr().cast_mut(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(());
    }
    Ok(())
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide(SERVICE_NAME);
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), ptr::null()) };
    if handle.is_null() {
        return;
    }
    STATUS_HANDLE.store(handle as usize, Ordering::Release);
    report(
        SERVICE_START_PENDING,
        NO_ERROR,
        START_WAIT_HINT_MILLISECONDS,
    );
    let exit_code = match run_body() {
        Ok(()) => NO_ERROR,
        Err(error) => error.raw_os_error().map_or(1, |code| code as u32),
    };
    report(SERVICE_STOPPED, exit_code, 0);
}

fn run_body() -> io::Result<()> {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let stop = Arc::new(unsafe { OwnedHandle::from_raw_handle(event) });
    let _ = STOP_EVENT.set(stop.clone());
    let directory = state_dir::service_directory()?;
    state_dir::ensure_private_directory(&directory)?;
    // `<install>\current`: the updater's `--target`, whose `runtime` holds this binary.
    let package_root = std::env::current_exe()?
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| io::Error::other("service executable has no package root"))?;
    let branding = (directory.clone(), package_root.clone());
    let troubleshooter = Arc::new(ServiceTroubleshooter {
        directory: directory.clone(),
    });
    let service = Arc::new(Mutex::new(
        ServiceLoop::new(ProductionCores::new(directory.clone())?)
            .with_log_directory(directory.clone())
            .with_updater(Box::new(ProductionUpdater::new(
                directory,
                package_root,
                stop.clone(),
            )))
            .with_troubleshooter(troubleshooter),
    ));
    let ticker = {
        let (service, stop) = (service.clone(), stop.clone());
        thread::Builder::new()
            .name("multicore-service-tick".into())
            .spawn(move || {
                while unsafe { WaitForSingleObject(stop.as_raw_handle(), TICK_MILLISECONDS) }
                    == WAIT_TIMEOUT
                {
                    service
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .tick();
                }
            })?
    };
    report(SERVICE_RUNNING, NO_ERROR, 0);
    // Installs updated in place from 1.1.1 still carry the "MultiCore" names.
    super::branding::spawn(branding.0, branding.1);
    let result = serve(
        SERVICE_PIPE_NAME,
        service.clone(),
        production_verifier(std::env::current_exe()?),
        stop.clone(),
    );
    // Also ends the ticker and every connection thread when `serve` failed on its own.
    unsafe { SetEvent(stop.as_raw_handle()) };
    let _ = ticker.join();
    service
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .stop_everything();
    result
}

/// `ServiceCommand::Troubleshoot`: the fixed fixes of `crate::troubleshoot`, each logged to
/// `logs/service.log`.
struct ServiceTroubleshooter {
    directory: std::path::PathBuf,
}

impl Troubleshooter for ServiceTroubleshooter {
    fn run(
        &self,
        fixes: &[multicore_core::troubleshoot::PrivilegedFix],
    ) -> Vec<multicore_core::troubleshoot::FixOutcome> {
        crate::troubleshoot::run_batch(fixes, &mut |line| {
            super::service_log::append(&self.directory, line);
        })
    }
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, NO_ERROR, STOP_WAIT_HINT_MILLISECONDS);
            if let Some(stop) = STOP_EVENT.get() {
                unsafe { SetEvent(stop.as_raw_handle()) };
            }
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}
