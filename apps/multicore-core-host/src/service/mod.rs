//! Always-ready service mode: `multicore-core-host --service`.

pub(crate) mod controller_proxy;
pub(crate) mod core_hashes;
pub(crate) mod core_loop;
pub(crate) mod cores;
pub(crate) mod mihomo_rest;
pub(crate) mod state_dir;
#[cfg(unix)]
pub(crate) mod unix_identity;
#[cfg(unix)]
pub(crate) mod unix_server;

/// Serves `/run/multicore/service.sock` until the process is stopped (systemd `multicore.service`).
#[cfg(unix)]
pub(crate) fn run() -> Result<(), ()> {
    unix_server::run_service()
}
