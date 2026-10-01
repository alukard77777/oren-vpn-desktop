//! Always-ready service mode: `multicore-core-host --service`.

#[cfg(windows)]
pub(crate) mod branding;
pub(crate) mod controller_proxy;
/// Windows only: Linux cores come from the root-owned `/usr/lib/multicore`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod core_hashes;
pub(crate) mod core_loop;
pub(crate) mod cores;
pub(crate) mod mihomo_rest;
pub(crate) mod service_log;
pub(crate) mod state_dir;
#[cfg(unix)]
pub(crate) mod unix_identity;
#[cfg(unix)]
pub(crate) mod unix_server;
/// Windows only: Linux packages are updated by the package manager.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod update;
#[cfg(windows)]
pub(crate) mod windows_identity;
#[cfg(windows)]
pub(crate) mod windows_scm;
#[cfg(windows)]
pub(crate) mod windows_server;

use std::ffi::OsString;

/// `multicore-core-host --service` and nothing else selects service mode.
pub(crate) fn wants_service_mode(arguments: &[OsString]) -> bool {
    arguments.len() == 1 && arguments[0] == "--service"
}

/// This process's arguments after the program name, only for `wants_service_mode`.
pub(crate) fn process_arguments() -> Vec<OsString> {
    std::env::args_os().skip(1).collect()
}

/// Runs the MultiCore Windows service under the SCM dispatcher.
#[cfg(windows)]
pub(crate) fn run() -> Result<(), ()> {
    windows_scm::run()
}

/// Serves `/run/multicore/service.sock` until the process is stopped (systemd `multicore.service`).
#[cfg(unix)]
pub(crate) fn run() -> Result<(), ()> {
    unix_server::run_service()
}
