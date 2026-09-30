//! Always-ready service mode: `multicore-core-host --service`.

pub(crate) mod controller_proxy;
pub(crate) mod core_hashes;
pub(crate) mod core_loop;
pub(crate) mod cores;
pub(crate) mod mihomo_rest;
pub(crate) mod state_dir;
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

#[cfg(windows)]
pub(crate) fn run() -> Result<(), ()> {
    windows_scm::run()
}

/// Replaced by the Linux service server (Task 8).
#[cfg(unix)]
pub(crate) fn run() -> Result<(), ()> {
    Err(())
}
