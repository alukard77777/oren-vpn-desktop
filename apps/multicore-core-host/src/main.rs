#[cfg(any(windows, unix))]
mod runtime_host;
#[cfg(windows)]
mod runtime_paths;
/// `multicore-core-host --service`: the always-ready Windows service / systemd unit.
#[cfg(any(windows, unix))]
mod service;
/// `multicore-core-host --troubleshoot <fix>...`: the UAC-elevated troubleshooting helper.
#[cfg(windows)]
mod troubleshoot;
#[cfg(unix)]
mod unix_runtime_paths;
#[cfg(unix)]
mod unix_socket;
#[cfg(windows)]
mod windows_pipe;
#[cfg(windows)]
mod wintun_cleanup;

#[cfg(any(windows, unix))]
fn main() {
    #[cfg(windows)]
    {
        let arguments = service::process_arguments();
        if troubleshoot::wants_helper(&arguments) {
            std::process::exit(troubleshoot::helper_main(&arguments) as i32);
        }
    }
    if service::wants_service_mode(&service::process_arguments()) {
        if service::run().is_err() {
            eprintln!("MultiCore core host: service stopped safely.");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    let result = windows_pipe::run().map_err(drop);
    #[cfg(unix)]
    let result = unix_socket::run().map_err(drop);
    if result.is_err() {
        eprintln!("MultiCore core host: privileged broker stopped safely.");
        std::process::exit(1);
    }
}

#[cfg(not(any(windows, unix)))]
fn main() {
    eprintln!("MultiCore core host is not available on this platform.");
    std::process::exit(1);
}
