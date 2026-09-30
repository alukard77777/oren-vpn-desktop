//! Always-ready service mode: `multicore-core-host --service`.

pub(crate) mod controller_proxy;
pub(crate) mod core_hashes;
pub(crate) mod core_loop;
pub(crate) mod cores;
pub(crate) mod mihomo_rest;
pub(crate) mod state_dir;
#[cfg(windows)]
pub(crate) mod windows_identity;
#[cfg(windows)]
pub(crate) mod windows_server;
