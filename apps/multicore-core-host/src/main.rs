#[cfg(any(windows, unix))]
mod runtime_host;
#[cfg(windows)]
mod runtime_paths;
/// `multicore-core-host --service`. The Unix server arrives with the Linux service (Task 8);
/// until then the Unix build only compiles the platform-neutral parts.
#[cfg(any(windows, unix))]
#[cfg_attr(unix, allow(dead_code))]
mod service;
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
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if service::wants_service_mode(&arguments) {
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
