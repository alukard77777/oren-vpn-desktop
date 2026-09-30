//! `/run/multicore/service.sock`: one thread per connection, all sharing one service loop.
//! systemd's `KillMode=control-group` stops the cores together with the service.

use std::{
    fs, io,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use multicore_core::{
    elevation_protocol::{BrokerErrorCode, read_stream_frame},
    service_protocol::{
        ServiceCommand, ServiceResponse, decode_service_command, write_service_frame,
    },
};

use super::core_loop::{ClientIdentity, ConnectionId, Cores, ServiceLoop, execute_shared};
use super::cores::ProductionCores;
use super::state_dir;
use super::unix_identity::{IdentityError, production_identity};

pub(crate) const SOCKET_PATH: &str = "/run/multicore/service.sock";
const TICK: Duration = Duration::from_secs(2);
/// Back-off after a failed `accept` (for example `EMFILE`), so the loop never spins.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

pub(crate) type Verifier =
    dyn Fn(&UnixStream) -> Result<ClientIdentity, IdentityError> + Send + Sync;

/// Binds `path` (replacing a stale socket in the root-owned runtime directory) with mode 0666:
/// every local user may connect, identity is checked per connection.
pub(crate) fn bind(path: &Path) -> io::Result<UnixListener> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

pub(crate) fn serve<C: Cores + 'static>(
    listener: UnixListener,
    service: Arc<Mutex<ServiceLoop<C>>>,
    verifier: Arc<Verifier>,
) -> io::Result<()> {
    let next = AtomicU64::new(1);
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            thread::sleep(ACCEPT_RETRY);
            continue;
        };
        let id = ConnectionId(next.fetch_add(1, Ordering::Relaxed));
        let (service, verifier) = (service.clone(), verifier.clone());
        thread::Builder::new()
            .name("multicore-service-connection".into())
            .spawn(move || handle_connection(stream, id, service, verifier))?;
    }
    Ok(())
}

fn handle_connection<C: Cores>(
    mut stream: UnixStream,
    id: ConnectionId,
    service: Arc<Mutex<ServiceLoop<C>>>,
    verifier: Arc<Verifier>,
) {
    let Ok(identity) = verifier(&stream) else {
        let _ = write_service_frame(
            &mut stream,
            &ServiceResponse::Error {
                code: BrokerErrorCode::AuthenticationFailed,
            },
        );
        return;
    };
    while let Ok(frame) = read_stream_frame(&mut stream) {
        let Ok(command) = decode_service_command(&frame) else {
            let _ = write_service_frame(
                &mut stream,
                &ServiceResponse::Error {
                    code: BrokerErrorCode::InvalidRequest,
                },
            );
            break;
        };
        let shutdown = command == ServiceCommand::Shutdown;
        let response = execute_shared(&service, id, &identity, command);
        if write_service_frame(&mut stream, &response).is_err() || shutdown {
            break;
        }
    }
    service
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .client_gone(id);
}

pub(crate) fn run_service() -> Result<(), ()> {
    let directory = state_dir::service_directory().map_err(drop)?;
    state_dir::ensure_private_directory(&directory).map_err(drop)?;
    let service = Arc::new(Mutex::new(ServiceLoop::new(
        ProductionCores::new(directory).map_err(drop)?,
    )));
    {
        let service = service.clone();
        thread::Builder::new()
            .name("multicore-service-tick".into())
            .spawn(move || {
                loop {
                    thread::sleep(TICK);
                    service
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .tick();
                }
            })
            .map_err(drop)?;
    }
    let listener = bind(Path::new(SOCKET_PATH)).map_err(drop)?;
    serve(listener, service, Arc::new(production_identity)).map_err(drop)
}
