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
use multicore_daemon::connection_mode::ModeController;
#[cfg(not(windows))]
use multicore_daemon::{
    connection_mode::NoProxyController, elevated_controller::ProductionElevatedController,
};

/// The daemon's connection controller: elevated TUN, or (Windows) the local proxy mode.
#[cfg(windows)]
type Controller = proxy_mode::Controller;
#[cfg(not(windows))]
type Controller = ModeController<ProductionElevatedController, NoProxyController>;
use multicore_daemon::elevated_controller::{production_factory, runtime_generation_from_path};
use multicore_daemon::{
    BackendError, ConnectOptions, CoreBackend, MihomoHttpSelector, PreparedController,
    bind_loopback, options_controller_factory, publish_readiness_if_enabled, try_router,
    try_router_with_dashboard_url,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    multicore_core::ensure_crypto_provider_installed();
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

    // The vendored zashboard build ships as a `dashboard/` sibling of `cores/` and `runtime/` at
    // the package root in a packaged release (see packaging/*/versions.json and
    // apps/multicore-core-host/src/runtime_paths.rs, which resolves the same directory the same
    // way). This executable itself lives one level deeper, inside `runtime/`, so the package
    // root is its *grandparent*, not its parent. The directory is absent in ad hoc dev/test
    // builds, in which case the dashboard feature is simply unavailable: no runtime download is
    // ever attempted. `dashboard_url` embeds the per-process controller secret and must never be
    // logged; it only leaves this process through the authenticated `/v1/dashboard-url` route.
    let dashboard_directory = env::current_exe().ok().and_then(|executable| {
        dashboard_directory_for(&executable).map(|package_root| package_root.join("dashboard"))
    });
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
    // A proxy left pointing at a dead Mihomo by a crashed session is put back before anything
    // else runs (see `system_proxy`).
    #[cfg(windows)]
    let system_proxy_marker = data_directory.join("system-proxy.json");
    #[cfg(windows)]
    if let Err(error) = multicore_daemon::system_proxy::recover(
        &multicore_daemon::system_proxy::WinInetSettings,
        &system_proxy_marker,
    ) {
        eprintln!("system proxy recovery failed: {error}");
    }
    #[cfg(windows)]
    let proxy_mode = proxy_mode::ProxyMode {
        package_root: env::current_exe()
            .ok()
            .and_then(|executable| dashboard_directory_for(&executable)),
        dashboard_directory: dashboard_directory.clone(),
        log_path: data_directory.join("logs").join("latest-core.log"),
        marker_path: system_proxy_marker,
    };
    let elevated_factory = Mutex::new(None);
    let controller_factory =
        options_controller_factory(move |snapshot: &Snapshot, options: ConnectOptions| {
            let xray_outbound_interface =
                default_ipv4_interface().map_err(|_| BackendError::new())?;
            let control = runtime_control
                .clone()
                .with_xray_outbound_interface(xray_outbound_interface)
                .map_err(|_| BackendError::new())?;
            // TUN off: cores run as this user, no administrator prompt (Windows only; elsewhere
            // the option is ignored and TUN is used).
            #[cfg(windows)]
            if !options.tun {
                return proxy_mode.prepare(snapshot, &runtime_root, control);
            }
            #[cfg(not(windows))]
            let _ = options;
            let staged = stage_runtime_with_mihomo_control(snapshot, &runtime_root, &control)
                .map_err(|_| BackendError::new())?;
            let generation = runtime_generation_from_path(&staged.paths().directory)
                .map_err(|_| BackendError::new())?;
            let mut elevated_factory = elevated_factory.lock().map_err(|_| BackendError::new())?;
            if elevated_factory.is_none() {
                *elevated_factory = Some(production_factory().map_err(|_| BackendError::new())?);
            }
            let elevated_factory = elevated_factory.as_ref().ok_or_else(BackendError::new)?;
            let controller: Arc<Controller> = Arc::new(ModeController::Tun(
                elevated_factory
                    .controller(generation)
                    .map_err(|_| BackendError::new())?,
            ));
            Ok(PreparedController::with_runtime(controller, staged))
        });
    // Persisted so a real subscription-add failure's diagnostics survive a
    // daemon restart between the failure and a user exporting the log via
    // "Копировать лог" - without this, the in-memory event buffer was empty
    // by the time anyone checked it (see the "Событий пока нет" bug report).
    let events_log_path = data_directory.join("logs").join("events.log");
    let backend = CoreBackend::new_with_connect_options(
        store,
        fetcher,
        controller_factory,
        selector,
        Some(events_log_path),
    )?;

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

/// The package root (parent of both `runtime/`, which this executable lives
/// in, and `cores/`), where an optional vendored `dashboard/` directory is
/// staged as a sibling of `cores/` - not as a sibling of this executable
/// itself, which is one level deeper. Matches
/// `apps/multicore-core-host/src/runtime_paths.rs`'s resolution of the same
/// directory.
/// System-proxy mode (TUN off) on Windows: the daemon runs the cores itself, as the logged-in
/// user, in a kill-on-close job, and points the user's proxy settings at Mihomo while it runs.
#[cfg(windows)]
mod proxy_mode {
    use std::{net::TcpListener, path::PathBuf, sync::Arc};

    use multicore_core::{
        CORE_READINESS_TIMEOUT, CoreLogBuffer, MihomoRuntimeControl, RuntimePaths,
        SidecarProcessController, Snapshot, WindowsJobLauncher, stage_runtime_with_mihomo_control,
    };
    use multicore_daemon::{
        BackendError, PreparedController,
        connection_mode::{ModeController, SystemProxyController},
        elevated_controller::ProductionElevatedController,
        system_proxy::{SystemProxy, WinInetSettings},
    };

    pub(crate) type LocalController =
        SystemProxyController<SidecarProcessController<WindowsJobLauncher>>;
    pub(crate) type Controller = ModeController<ProductionElevatedController, LocalController>;

    pub(crate) struct ProxyMode {
        pub(crate) package_root: Option<PathBuf>,
        pub(crate) dashboard_directory: Option<PathBuf>,
        pub(crate) log_path: PathBuf,
        pub(crate) marker_path: PathBuf,
    }

    impl ProxyMode {
        pub(crate) fn prepare(
            &self,
            snapshot: &Snapshot,
            runtime_root: &std::path::Path,
            control: MihomoRuntimeControl,
        ) -> Result<PreparedController<Controller>, BackendError> {
            let port = free_loopback_port().ok_or_else(BackendError::new)?;
            let control = control
                .with_proxy_mode(port)
                .map_err(|_| BackendError::new())?;
            let staged = stage_runtime_with_mihomo_control(snapshot, runtime_root, &control)
                .map_err(|_| BackendError::new())?;
            let cores = self
                .package_root
                .as_ref()
                .ok_or_else(BackendError::new)?
                .join("cores");
            let paths = RuntimePaths {
                xray_binary: cores.join("xray.exe"),
                mihomo_binary: cores.join("mihomo.exe"),
                xray_config: staged.paths().xray_config.clone(),
                mihomo_config: staged.paths().mihomo_config.clone(),
                dashboard_directory: self
                    .dashboard_directory
                    .clone()
                    .filter(|directory| directory.join("index.html").is_file()),
                mihomo_home_directory: None,
            };
            let logs = Arc::new(CoreLogBuffer::persistent_or_memory(&self.log_path));
            let launcher =
                WindowsJobLauncher::for_runtime(paths.clone(), logs, CORE_READINESS_TIMEOUT)
                    .map_err(|_| BackendError::new())?;
            let proxy = SystemProxy::new(Box::new(WinInetSettings), self.marker_path.clone(), port);
            let controller = Arc::new(ModeController::Proxy(SystemProxyController::new(
                SidecarProcessController::with_launcher(paths, launcher),
                Arc::new(proxy),
            )));
            Ok(PreparedController::with_runtime(controller, staged))
        }
    }

    /// A loopback port free right now; Mihomo binds it moments later. A port taken in between
    /// fails Mihomo's readiness check and the connect reports it.
    fn free_loopback_port() -> Option<u16> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        listener.local_addr().ok().map(|address| address.port())
    }
}

fn dashboard_directory_for(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    executable
        .parent()
        .and_then(|runtime_dir| runtime_dir.parent())
        .map(std::path::Path::to_path_buf)
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
    use super::dashboard_directory_for;

    const MAIN_SOURCE: &str = include_str!("main.rs");

    #[test]
    fn dashboard_directory_is_a_sibling_of_cores_not_of_this_executable() {
        // multicore-daemon.exe lives in `<package>/runtime/`; the vendored
        // dashboard is staged at `<package>/dashboard/`, the same package
        // root apps/multicore-core-host/src/runtime_paths.rs resolves `cores/`
        // from. Regression coverage for a real bug: an earlier version only
        // went up one `.parent()`, landing on `<package>/runtime/dashboard`,
        // which never exists, so the dashboard silently never worked despite
        // package-windows-release.ps1 staging it correctly.
        #[cfg(windows)]
        let executable = std::path::Path::new(
            r"C:\Users\Example\AppData\Local\Programs\MultiCore\current\runtime\multicore-daemon.exe",
        );
        #[cfg(not(windows))]
        let executable = std::path::Path::new("/usr/lib/multicore/runtime/multicore-daemon");

        let package_root = dashboard_directory_for(executable).expect("package root resolves");
        assert_eq!(package_root, executable.parent().unwrap().parent().unwrap());
        assert_ne!(
            package_root.join("dashboard"),
            executable.parent().unwrap().join("dashboard"),
            "must not resolve to a sibling of the executable itself"
        );
    }

    #[test]
    fn live_dns_does_not_resolve_and_pin_endpoint_ips_during_connect() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("to_socket_addrs"));
        assert!(!production.contains("with_xray_resolved_hosts"));
    }

    #[test]
    fn production_daemon_runs_tun_elevated_and_proxy_mode_only_with_the_system_proxy() {
        let production = MAIN_SOURCE.split("#[cfg(test)]").next().unwrap();
        let (tun_path, proxy_mode) = production
            .split_once("mod proxy_mode {")
            .expect("proxy mode module");
        // TUN mode: cores only ever start in the elevated host.
        assert!(tun_path.contains("elevated_factory"));
        assert!(tun_path.contains(".controller(generation)"));
        assert!(tun_path.contains("runtime_generation_from_path"));
        assert!(tun_path.contains("if !options.tun {"));
        for forbidden in ["SidecarProcessController", "RuntimePaths"] {
            assert!(
                !tun_path.contains(forbidden),
                "the TUN path must not launch cores itself: {forbidden}"
            );
        }
        // Proxy mode: the package's own cores, TUN disabled, system proxy tied to Mihomo.
        assert!(proxy_mode.contains(".with_proxy_mode(port)"));
        assert!(proxy_mode.contains("SystemProxyController::new("));
        assert!(proxy_mode.contains(".join(\"cores\")"));
        for forbidden in ["MULTICORE_XRAY_BIN", "MULTICORE_MIHOMO_BIN"] {
            assert!(
                !production.contains(forbidden),
                "core binaries must come from the package, not the environment: {forbidden}"
            );
        }
        // A crashed session's proxy is restored before the daemon serves anything.
        assert!(
            tun_path.find("system_proxy::recover(").unwrap()
                < tun_path.find("axum::serve(").unwrap()
        );
    }
}
