//! `\\.\pipe\MultiCore.Service.v2`: one thread per connection, all sharing one service loop.

use std::{
    ffi::OsStr,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use multicore_core::{
    elevation_protocol::{BrokerErrorCode, MAX_ELEVATION_FRAME_BYTES},
    service_protocol::{
        ServiceCommand, ServiceResponse, decode_service_command, encode_service_frame,
    },
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
        LocalFree, WAIT_OBJECT_0,
    },
    Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    },
    Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
        Threading::{INFINITE, WaitForMultipleObjects},
    },
};

use super::core_loop::{ClientIdentity, ConnectionId, Cores, ServiceLoop, execute_shared};
use super::windows_identity::{
    IdentityError, client_facts, expected_daemon_image, user_data_root, verify_client,
};
use crate::windows_pipe::{
    IO_TIMEOUT, PendingIo, complete_pending, read_message, read_message_or_signal, write_frame,
};

/// How long a freshly connected client may take to send its first command.
const FIRST_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) const SERVICE_PIPE_NAME: &str = r"\\.\pipe\MultiCore.Service.v2";
/// What an interactive client may do with the pipe: `FILE_READ_DATA | FILE_WRITE_DATA |
/// FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE`. Deliberately without
/// `FILE_APPEND_DATA` (= `FILE_CREATE_PIPE_INSTANCE`), so no user can add server instances of
/// the service pipe and catch other users' daemons. `FILE_WRITE_ATTRIBUTES` is what
/// `SetNamedPipeHandleState` needs. The daemon opens the pipe with exactly these rights.
#[allow(
    dead_code,
    reason = "pinned by the tests; the daemon opens with the same rights"
)]
pub(crate) const CLIENT_PIPE_ACCESS: u32 = 0x0010_0183;
/// Protected DACL: FILE_GENERIC_READ | FILE_GENERIC_WRITE for SYSTEM and Administrators,
/// [`CLIENT_PIPE_ACCESS`] for interactive users (INTERACTIVE, `S-1-5-4`). No Everyone,
/// Anonymous or Network entry.
pub(crate) const PIPE_SDDL: &str =
    "D:P(A;;0x0012019f;;;SY)(A;;0x0012019f;;;BA)(A;;0x00100183;;;IU)";

pub(crate) type Verifier = dyn Fn(HANDLE) -> Result<ClientIdentity, IdentityError> + Send + Sync;

/// One server instance of `name`. `first` adds `FILE_FLAG_FIRST_PIPE_INSTANCE`, so the service
/// refuses to start when someone else already owns the name.
#[allow(dead_code, reason = "the tests pin the production DACL through it")]
pub(crate) fn create_instance(name: &str, first: bool) -> io::Result<OwnedHandle> {
    create_instance_with(name, first, PIPE_SDDL)
}

/// [`create_instance`] with an explicit security descriptor (tests use narrower ones).
pub(crate) fn create_instance_with(name: &str, first: bool, sddl: &str) -> io::Result<OwnedHandle> {
    let sddl = OsStr::new(sddl)
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
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let wide = OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX
                | FILE_FLAG_OVERLAPPED
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                },
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            MAX_ELEVATION_FRAME_BYTES as u32,
            MAX_ELEVATION_FRAME_BYTES as u32,
            0,
            &security,
        )
    };
    let error = io::Error::last_os_error();
    unsafe { LocalFree(descriptor.cast()) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(error);
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

enum Accepted {
    Connected,
    Stopped,
}

fn wait_connect(pipe: HANDLE, stop: HANDLE) -> io::Result<Accepted> {
    let mut pending =
        PendingIo::with_buffer(Vec::new()).map_err(|_| io::Error::other("pipe event"))?;
    if unsafe { ConnectNamedPipe(pipe, pending.overlapped_mut()) } != 0 {
        return Ok(Accepted::Connected);
    }
    match unsafe { GetLastError() } {
        ERROR_PIPE_CONNECTED => Ok(Accepted::Connected),
        ERROR_IO_PENDING => {
            let handles = [pending.event_handle(), stop];
            if unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) } == WAIT_OBJECT_0
            {
                complete_pending(pipe, &mut pending)
                    .map(|_| Accepted::Connected)
                    .map_err(|_| io::Error::other("pipe connection failed"))
            } else {
                pending.cancel_and_drain(pipe);
                Ok(Accepted::Stopped)
            }
        }
        error => Err(io::Error::from_raw_os_error(error as i32)),
    }
}

/// Accepts clients until `stop` (a manual-reset event) is signalled. Every connection runs on
/// its own thread and ends when its client leaves or `stop` is signalled.
pub(crate) fn serve<C: Cores + 'static>(
    name: &str,
    service: Arc<Mutex<ServiceLoop<C>>>,
    verifier: Arc<Verifier>,
    stop: Arc<OwnedHandle>,
) -> io::Result<()> {
    serve_with(name, PIPE_SDDL, service, verifier, stop)
}

/// [`serve`] with an explicit pipe DACL. The service DACL lets only SYSTEM and Administrators
/// create instances, so tests running as a standard user pass one that includes them.
pub(crate) fn serve_with<C: Cores + 'static>(
    name: &str,
    sddl: &str,
    service: Arc<Mutex<ServiceLoop<C>>>,
    verifier: Arc<Verifier>,
    stop: Arc<OwnedHandle>,
) -> io::Result<()> {
    let next = AtomicU64::new(1);
    let mut first = true;
    loop {
        let instance = create_instance_with(name, first, sddl)?;
        first = false;
        match wait_connect(instance.as_raw_handle(), stop.as_raw_handle()) {
            Ok(Accepted::Stopped) => return Ok(()),
            Ok(Accepted::Connected) => {}
            // A client that vanished between connecting and being accepted.
            Err(_) => continue,
        }
        let id = ConnectionId(next.fetch_add(1, Ordering::Relaxed));
        let (service, verifier, stop) = (service.clone(), verifier.clone(), stop.clone());
        thread::Builder::new()
            .name("multicore-service-connection".into())
            .spawn(move || handle_connection(instance, id, service, verifier, stop))?;
    }
}

fn reply(pipe: HANDLE, response: &ServiceResponse) -> bool {
    encode_service_frame(response)
        .ok()
        .is_some_and(|frame| write_frame(pipe, frame, IO_TIMEOUT).is_ok())
}

fn handle_connection<C: Cores>(
    instance: OwnedHandle,
    id: ConnectionId,
    service: Arc<Mutex<ServiceLoop<C>>>,
    verifier: Arc<Verifier>,
    stop: Arc<OwnedHandle>,
) {
    let pipe = instance.as_raw_handle();
    // The client is identified once it sent its first command: impersonating it (to bind the
    // connection to its token) needs data read from the pipe. A silent client is dropped.
    let Ok(first) = read_message(pipe, FIRST_COMMAND_TIMEOUT) else {
        unsafe { DisconnectNamedPipe(pipe) };
        return;
    };
    let Ok(identity) = verifier(pipe) else {
        let _ = reply(
            pipe,
            &ServiceResponse::Error {
                code: BrokerErrorCode::AuthenticationFailed,
            },
        );
        unsafe { DisconnectNamedPipe(pipe) };
        return;
    };
    let mut pending = Some(first);
    while let Some(frame) = pending
        .take()
        .or_else(|| read_message_or_signal(pipe, stop.as_raw_handle()).ok())
    {
        let Ok(command) = decode_service_command(&frame) else {
            let _ = reply(
                pipe,
                &ServiceResponse::Error {
                    code: BrokerErrorCode::InvalidRequest,
                },
            );
            break;
        };
        let shutdown = command == ServiceCommand::Shutdown;
        let response = execute_shared(&service, id, &identity, command);
        if !reply(pipe, &response) || shutdown {
            break;
        }
    }
    service
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .client_gone(id);
    unsafe { DisconnectNamedPipe(pipe) };
}

/// The production check: the installed daemon next to this service, an interactive logon, and
/// the user's data directory from the client's own token.
pub(crate) fn production_verifier(host_executable: PathBuf) -> Arc<Verifier> {
    Arc::new(move |pipe| {
        let expected = expected_daemon_image(&host_executable).ok_or(IdentityError::Unavailable)?;
        let (facts, token) = client_facts(pipe)?;
        verify_client(&facts, &expected)?;
        let data_root = user_data_root(token.as_raw_handle())?;
        Ok(ClientIdentity {
            user_key: format!("{}:{}", facts.session_id, facts.user_sid),
            data_root,
            uid: 0,
        })
    })
}
