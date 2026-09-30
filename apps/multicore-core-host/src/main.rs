#[cfg(any(windows, unix))]
mod runtime_host;
#[cfg(windows)]
mod runtime_paths;
/// Wired into `main` by the `--service` dispatch (Task 7); until then only tests reach it.
#[cfg(any(windows, unix))]
#[allow(dead_code)]
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
