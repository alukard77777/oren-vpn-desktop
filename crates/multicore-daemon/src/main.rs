use std::{
    env, io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use multicore_core::{
    DeviceIdentity, MihomoRuntimeControl, ProfileStore, ReqwestHttpClient, Snapshot,
    SubscriptionFetcher, default_ipv4_interface, stage_runtime_with_mihomo_control,
};
use multicore_daemon::elevated_controller::{production_factory, runtime_generation_from_path};
use multicore_daemon::{
    BackendError, CoreBackend, MihomoHttpSelector, PreparedController, bind_loopback,
    publish_readiness_if_enabled, transactional_controller_factory, try_router,
    try_router_with_dashboard_url,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = required_env("MULTICORE_DAEMON_TOKEN")?;
    let address: SocketAddr = env::var("MULTICORE_DAEMON_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
        .parse()?;
    let data_directory = PathBuf::from(required_env("MULTICORE_DAEMON_DATA_DIR")?);
    let device_identity = DeviceIdentity::load_or_create(&data_directory)?;
    let mihomo_controller_address: SocketAddr = env::var("MULTICORE_MIHOMO_CONTROLLER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:19090".to_owned())
        .parse()?;
    if !mihomo_controller_address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MULTICORE_MIHOMO_CONTROLLER_ADDR must be an IP loopback SocketAddr",
        )
        .into());
    }
    let mut secret_bytes = [0_u8; 32];
    getrandom::fill(&mut secret_bytes)?;
    let mihomo_controller_secret: String = secret_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let runtime_control =
        MihomoRuntimeControl::new(mihomo_controller_address, mihomo_controller_secret.clone())?;
    let selector = Arc::new(MihomoHttpSelector::new(
        mihomo_controller_address,
        &mihomo_controller_secret,
    )?);

    // The vendored zashboard build ships as a `dashboard/` sibling of this executable in a
    // packaged release (see packaging/*/versions.json). It is absent in ad hoc dev/test builds,
    // in which case the dashboard feature is simply unavailable: no runtime download is ever
    // attempted. `dashboard_url` embeds the per-process controller secret and must never be
    // logged; it only leaves this process through the authenticated `/v1/dashboard-url` route.
    let dashboard_directory = env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|parent| parent.join("dashboard")));
    let runtime_control = match &dashboard_directory {
        Some(directory) => runtime_control
            .clone()
            .with_external_ui_directory(directory)
            .unwrap_or(runtime_control),
        None => runtime_control,
    };
    let dashboard_url: Option<Arc<str>> = dashboard_directory
        .as_deref()
        .filter(|directory| directory.join("index.html").is_file())
        .map(|_| {
            Arc::from(format!(
                "http://{mihomo_controller_address}/ui/#/setup?hostname={}&port={}&secret={mihomo_controller_secret}",
                mihomo_controller_address.ip(),
                mihomo_controller_address.port(),
            ))
        });
    drop(mihomo_controller_secret);

    let store = ProfileStore::open(
        data_directory.join("profiles"),
        Some(data_directory.join("snapshots")),
    )?;
    let fetcher = SubscriptionFetcher::new(ReqwestHttpClient::new(device_identity)?);
    let runtime_root = data_directory.join("runtime");
    let elevated_factory = Mutex::new(None);
    let controller_factory = transactional_controller_factory(move |snapshot: &Snapshot| {
        let xray_outbound_interface = default_ipv4_interface().map_err(|_| BackendError::new())?;
        let control = runtime_control
            .clone()
            .with_xray_outbound_interface(xray_outbound_interface)
            .map_err(|_| BackendError::new())?;
        let staged = stage_runtime_with_mihomo_control(snapshot, &runtime_root, &control)
            .map_err(|_| BackendError::new())?;
        let generation = runtime_generation_from_path(&staged.paths().directory)
            .map_err(|_| BackendError::new())?;
        let mut elevated_factory = elevated_factory.lock().map_err(|_| BackendError::new())?;
        if elevated_factory.is_none() {
            *elevated_factory = Some(production_factory().map_err(|_| BackendError::new())?);
        }
        let elevated_factory = elevated_factory.as_ref().ok_or_else(BackendError::new)?;
        let controller = Arc::new(
            elevated_factory
                .controller(generation)
                .map_err(|_| BackendError::new())?,
        );
        Ok(PreparedController::with_runtime(controller, staged))
    });
    let backend =
        CoreBackend::new_transactional_with_selector(store, fetcher, controller_factory, selector)?;

    let app = match dashboard_url {
        Some(dashboard_url) => try_router_with_dashboard_url(backend, token, dashboard_url)?,
        None => try_router(backend, token)?,
    };
    let listener = bind_loopback(address).await?;
    let resolved_address = listener.local_addr()?;
    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        publish_readiness_if_enabled(
            &mut stdout,
            resolved_address,
            env::var_os("MULTICORE_DAEMON_READY_STDOUT").as_deref(),
        )?;
    }
    axum::serve(listener, app).await?;
    Ok(())
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("задайте переменную окружения {name} перед запуском daemon"),
            )
        })
}

#[cfg(test)]
mod tests {
    const MAIN_SOURCE: &str = include_str!("main.rs");

    #[test]
    fn live_dns_does_not_resolve_and_pin_endpoint_ips_during_connect() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("to_socket_addrs"));
        assert!(!production.contains("with_xray_resolved_hosts"));
    }

    #[test]
    fn production_daemon_uses_only_the_elevated_core_controller() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains("elevated_factory"));
        assert!(production.contains(".controller(generation)"));
        assert!(production.contains("runtime_generation_from_path"));
        for forbidden in [
            "MULTICORE_XRAY_BIN",
            "MULTICORE_MIHOMO_BIN",
            "SidecarProcessController",
            "RuntimePaths",
        ] {
            assert!(
                !production.contains(forbidden),
                "production main still contains {forbidden}"
            );
        }
    }
}
