//! Production [`Cores`]: resolves the client's staged generation with the same guarded
//! resolver the on-demand host uses, copies it through the sanitiser into the service
//! directory, runs both cores from the admin-owned package with TUN off, and exposes Mihomo's
//! controller to the daemon only through the filtered [`ControllerProxy`] (plan amendment A1).

use std::{
    fs, io,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use multicore_core::{
    CORE_READINESS_TIMEOUT, CoreLogBuffer, DiagnosticStream, Engine, MAX_CONFIG_BYTES,
    MihomoServicePolicy, ProcessController, ProcessError, RuntimeCheckState, RuntimePaths,
    SidecarProcessController,
    elevation_protocol::{
        BrokerErrorCode, ElevatedDiagnosticStream, ElevatedEngine, ElevatedLogRecord,
        MAX_ELEVATION_FRAME_BYTES,
    },
    sanitize_mihomo, sanitize_xray,
    service_protocol::{ControllerEndpoint, ServiceResponse, encode_service_frame},
};
use zeroize::Zeroizing;

use super::controller_proxy::ControllerProxy;
use super::core_loop::{ClientIdentity, CoreHealth, Cores};
use super::{mihomo_rest, service_log, state_dir};
use crate::runtime_host::{broker_safe_log, state};

#[cfg(windows)]
use crate::runtime_paths::{ResolveError, ResolvedRuntime};
#[cfg(unix)]
use crate::unix_runtime_paths::{ResolveError, ResolvedRuntime};
#[cfg(unix)]
use multicore_core::UnixGroupLauncher as PlatformLauncher;
#[cfg(windows)]
use multicore_core::WindowsJobLauncher as PlatformLauncher;

/// How long `SetTun{on:true}` waits for the `MultiCore` adapter and Mihomo to agree.
pub(crate) const TUN_READY_TIMEOUT: Duration = Duration::from_secs(15);
const TUN_POLL: Duration = Duration::from_millis(100);
/// How long `SetTun{on:false}` waits for the loopback mixed port to accept connections again.
pub(crate) const MIXED_PORT_RECHECK: Duration = Duration::from_secs(3);

/// Whether something accepts TCP connections on `127.0.0.1:<port>` within `within`.
pub(crate) fn mixed_port_listens(port: u16, within: Duration) -> bool {
    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + within;
    loop {
        if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(TUN_POLL);
    }
}

struct ActiveCores {
    /// What the daemon gets: the proxy's port and session token, and the mixed port (0 for a
    /// TUN generation, which opens none).
    endpoint: ControllerEndpoint,
    /// Mihomo's own controller; never leaves the service.
    controller_port: u16,
    controller_secret: Zeroizing<String>,
    _proxy: ControllerProxy,
    controller: SidecarProcessController<PlatformLauncher>,
}

/// What `Prepare` takes from the client's staged generation.
#[derive(Debug)]
pub(crate) struct ClientGeneration {
    pub(crate) mihomo: Vec<u8>,
    pub(crate) xray: Vec<u8>,
    pub(crate) xray_binary: PathBuf,
    pub(crate) mihomo_binary: PathBuf,
}

/// Why [`read_client_generation`] refused: the code for the daemon, the step and the detail
/// for `logs/service.log`.
pub(crate) type Refusal = (BrokerErrorCode, &'static str, String);

/// Resolves `generation` of `client`, runs `verify_cores` and reads both configs through the
/// resolver's guarded handles.
///
/// The guards are released before this returns. They hold the client's data root, `runtime`,
/// the generation and their ancestors open sharing reads only; kept for as long as the cores
/// ran, they made every rename into `runtime` fail with os error 32, so the daemon could never
/// publish the next generation (a refresh or a new subscription failed until the daemon quit).
/// The cores run from the service's sanitised copies and never need the client's files again.
pub(crate) fn read_client_generation(
    client: &ClientIdentity,
    host_executable: &Path,
    generation: u64,
    verify_cores: impl FnOnce() -> Result<(), super::core_hashes::CoreHashError>,
) -> Result<ClientGeneration, Refusal> {
    use BrokerErrorCode::NotReady;
    let resolved = resolve_client(client, host_executable, generation)
        .map_err(|error| (NotReady, "resolve client runtime", format!("{error:?}")))?;
    verify_cores().map_err(|error| (NotReady, "verify core hashes", format!("{error:?}")))?;
    verify_unchanged(&resolved)
        .map_err(|error| (NotReady, "staged files changed", format!("{error:?}")))?;
    let mihomo = read_guarded(&resolved, &resolved.paths.mihomo_config)
        .map_err(|(code, error)| (code, "read mihomo.yaml", format!("{error:?}")))?;
    let xray = read_guarded(&resolved, &resolved.paths.xray_config)
        .map_err(|(code, error)| (code, "read xray.json", format!("{error:?}")))?;
    Ok(ClientGeneration {
        mihomo,
        xray,
        xray_binary: resolved.paths.xray_binary.clone(),
        mihomo_binary: resolved.paths.mihomo_binary.clone(),
    })
}

pub(crate) struct ProductionCores {
    runtime: tokio::runtime::Runtime,
    service_directory: PathBuf,
    host_executable: PathBuf,
    active: Option<ActiveCores>,
}

impl ProductionCores {
    /// `service_directory` must already be private (see `state_dir::ensure_private_directory`).
    pub(crate) fn new(service_directory: PathBuf) -> io::Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?,
            host_executable: std::env::current_exe()?,
            service_directory,
            active: None,
        })
    }

    fn launch(
        &mut self,
        client: &ClientIdentity,
        generation: u64,
    ) -> Result<ControllerEndpoint, BrokerErrorCode> {
        use BrokerErrorCode::{InvalidRequest, NotReady};
        let log = self.service_directory.clone();
        let refuse = |code: BrokerErrorCode, stage: &str, error: &dyn std::fmt::Debug| {
            service_log::append(
                &log,
                &format!(
                    "prepare generation {generation} refused ({code:?}) at {stage}: {error:?}"
                ),
            );
            code
        };
        let staged = read_client_generation(client, &self.host_executable, generation, || {
            self.verify_cores()
        })
        .map_err(|(code, stage, detail)| refuse(code, stage, &format_args!("{detail}")))?;
        let home = client_home(&self.service_directory, client);
        fs::create_dir_all(&home).map_err(|error| refuse(NotReady, "user home", &error))?;
        let controller_port = free_loopback_port()
            .ok_or_else(|| refuse(NotReady, "controller port", &"no free loopback port"))?;
        let controller_secret =
            random_secret().map_err(|error| refuse(NotReady, "controller secret", &error))?;
        let mihomo = sanitize_mihomo(
            &staged.mihomo,
            &MihomoServicePolicy {
                service_directory: &home,
                controller_port,
                controller_secret: &controller_secret,
            },
        )
        .map_err(|error| refuse(InvalidRequest, "sanitise mihomo.yaml", &error))?;
        let xray = sanitize_xray(&staged.xray)
            .map_err(|error| refuse(InvalidRequest, "sanitise xray.json", &error))?;
        let directory = state_dir::fresh_generation_directory(&self.service_directory, generation)
            .map_err(|error| refuse(NotReady, "generation directory", &error))?;
        fs::create_dir_all(home.join("providers"))
            .and_then(|()| fs::create_dir_all(self.service_directory.join("logs")))
            .map_err(|error| refuse(NotReady, "providers directory", &error))?;
        fs::write(directory.join("mihomo.yaml"), &mihomo.yaml)
            .and_then(|()| fs::write(directory.join("xray.json"), &xray))
            .map_err(|error| refuse(NotReady, "write sanitised configs", &error))?;
        let paths = RuntimePaths {
            xray_binary: staged.xray_binary,
            mihomo_binary: staged.mihomo_binary,
            xray_config: directory.join("xray.json"),
            mihomo_config: directory.join("mihomo.yaml"),
            dashboard_directory: None,
            mihomo_home_directory: Some(home),
        };
        let logs = Arc::new(CoreLogBuffer::persistent_or_memory(
            self.service_directory.join("logs").join("latest-core.log"),
        ));
        let launcher = PlatformLauncher::for_runtime(paths.clone(), logs, CORE_READINESS_TIMEOUT)
            .map_err(|error| refuse(NotReady, "core launcher", &error))?;
        // The unit's CAP_SYS_PTRACE / CAP_DAC_READ_SEARCH / CAP_SETPCAP are for the service's
        // own peer checks and staged-generation reads; Mihomo and Xray keep only CAP_NET_*.
        #[cfg(unix)]
        let launcher =
            launcher.with_restricted_capabilities(multicore_core::CoreCapabilities::SERVICE);
        let controller = SidecarProcessController::with_launcher(paths, launcher);
        for engine in [Engine::Xray, Engine::Mihomo] {
            if let Err(error) = self.runtime.block_on(controller.start(engine)) {
                self.stop_processes(&controller);
                return Err(refuse(start_error(&error), "start core", &error));
            }
        }
        let token = match random_secret() {
            Ok(token) => token,
            Err(error) => {
                self.stop_processes(&controller);
                return Err(refuse(NotReady, "proxy token", &error));
            }
        };
        let proxy = match self.runtime.block_on(ControllerProxy::start(
            controller_port,
            controller_secret.as_str().to_owned(),
            token.as_str().to_owned(),
        )) {
            Ok(proxy) => proxy,
            Err(error) => {
                self.stop_processes(&controller);
                return Err(refuse(NotReady, "controller proxy", &error));
            }
        };
        let endpoint = ControllerEndpoint {
            port: proxy.port(),
            secret: token.as_str().to_owned(),
            mixed_port: mihomo.mixed_port.unwrap_or(0),
        };
        self.active = Some(ActiveCores {
            endpoint: endpoint.clone(),
            controller_port,
            controller_secret,
            _proxy: proxy,
            controller,
        });
        Ok(endpoint)
    }

    /// Windows: refuses cores that no longer match the package's `SHA256SUMS.txt` (for example
    /// after a Mihomo self-upgrade). Linux cores come from the root-owned `/usr/lib/multicore`.
    fn verify_cores(&self) -> Result<(), super::core_hashes::CoreHashError> {
        #[cfg(windows)]
        {
            let package = self
                .host_executable
                .parent()
                .and_then(Path::parent)
                .ok_or(super::core_hashes::CoreHashError::Manifest)?;
            super::core_hashes::verify_core_hashes(
                package,
                &["cores/xray.exe", "cores/mihomo.exe"],
            )?;
        }
        Ok(())
    }

    fn stop_processes(&self, controller: &SidecarProcessController<PlatformLauncher>) {
        let _ = self.runtime.block_on(controller.stop(Engine::Mihomo));
        let _ = self.runtime.block_on(controller.stop(Engine::Xray));
    }
}

impl Cores for ProductionCores {
    fn prepare(
        &mut self,
        client: &ClientIdentity,
        generation: u64,
    ) -> Result<ControllerEndpoint, BrokerErrorCode> {
        self.stop_all();
        self.launch(client, generation)
    }

    fn restart(&mut self) -> Result<(), BrokerErrorCode> {
        let active = self.active.as_ref().ok_or(BrokerErrorCode::NotReady)?;
        self.stop_processes(&active.controller);
        if let Err(error) = self.verify_cores() {
            service_log::append(
                &self.service_directory,
                &format!("restart refused at verify core hashes: {error:?}"),
            );
            return Err(BrokerErrorCode::NotReady);
        }
        for engine in [Engine::Xray, Engine::Mihomo] {
            if let Err(error) = self.runtime.block_on(active.controller.start(engine)) {
                service_log::append(
                    &self.service_directory,
                    &format!("restart failed at start core: {error:?}"),
                );
                return Err(start_error(&error));
            }
        }
        Ok(())
    }

    fn set_tun(&mut self, on: bool) -> Result<(), BrokerErrorCode> {
        let active = self.active.as_ref().ok_or(BrokerErrorCode::NotReady)?;
        #[cfg(windows)]
        if on
            && let crate::wintun_cleanup::PhantomCleanup::Failed(code) =
                crate::wintun_cleanup::remove_phantom_tun_adapter()
        {
            eprintln!("stale TUN adapter could not be removed (CONFIGRET {code})");
        }
        let (port, secret) = (active.controller_port, active.controller_secret.as_str());
        mihomo_rest::set_tun_enabled(port, secret, on).map_err(|_| BrokerErrorCode::NotReady)?;
        if !on {
            // Switching TUN off re-creates Mihomo's listeners; a proxy-mode generation must come
            // back with its loopback mixed port, or the system proxy points at nothing.
            let mixed_port = active.endpoint.mixed_port;
            if mixed_port != 0 && !mixed_port_listens(mixed_port, MIXED_PORT_RECHECK) {
                return Err(BrokerErrorCode::NotReady);
            }
            return Ok(());
        }
        let deadline = Instant::now() + TUN_READY_TIMEOUT;
        loop {
            if self.runtime.block_on(multicore_core::multicore_tun_is_up())
                && mihomo_rest::tun_enabled(port, secret).unwrap_or(false)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BrokerErrorCode::TimedOut);
            }
            std::thread::sleep(TUN_POLL);
        }
    }

    fn tun_is_on(&mut self) -> Option<bool> {
        let active = self.active.as_ref()?;
        let secret = active.controller_secret.as_str();
        let enabled = mihomo_rest::tun_enabled(active.controller_port, secret).ok()?;
        Some(enabled || self.runtime.block_on(multicore_core::multicore_tun_is_up()))
    }

    fn health(&mut self) -> CoreHealth {
        let Some(active) = self.active.as_ref() else {
            return CoreHealth::default();
        };
        let diagnostics = self.runtime.block_on(active.controller.diagnostics());
        CoreHealth {
            xray: diagnostics.xray != RuntimeCheckState::Stopped,
            mihomo: diagnostics.mihomo != RuntimeCheckState::Stopped,
        }
    }

    fn stop_all(&mut self) {
        if let Some(active) = self.active.take() {
            self.stop_processes(&active.controller);
        }
    }

    fn diagnostics(&mut self) -> ServiceResponse {
        let Some(active) = self.active.as_ref() else {
            return ServiceResponse::ProcessDiagnostics {
                xray: state(RuntimeCheckState::Stopped),
                mihomo: state(RuntimeCheckState::Stopped),
                tun: state(RuntimeCheckState::Stopped),
                logs: Vec::new(),
            };
        };
        let diagnostics = self.runtime.block_on(active.controller.diagnostics());
        let tun = if self.runtime.block_on(multicore_core::multicore_tun_is_up()) {
            RuntimeCheckState::Ready
        } else {
            RuntimeCheckState::Stopped
        };
        let mut logs = diagnostics
            .logs
            .into_iter()
            .map(|record| ElevatedLogRecord {
                id: record.id,
                timestamp_ms: record.timestamp_ms,
                engine: match record.engine {
                    Engine::Xray => ElevatedEngine::Xray,
                    Engine::Mihomo => ElevatedEngine::Mihomo,
                },
                stream: match record.stream {
                    DiagnosticStream::Stdout => ElevatedDiagnosticStream::Stdout,
                    DiagnosticStream::Stderr => ElevatedDiagnosticStream::Stderr,
                },
                message: broker_safe_log(&record.message),
            })
            .collect::<Vec<_>>();
        loop {
            let response = ServiceResponse::ProcessDiagnostics {
                xray: state(diagnostics.xray),
                mihomo: state(diagnostics.mihomo),
                tun: state(tun),
                logs: logs.clone(),
            };
            let fits = encode_service_frame(&response)
                .is_ok_and(|frame| frame.len() <= MAX_ELEVATION_FRAME_BYTES);
            if fits || logs.is_empty() {
                return response;
            }
            logs.remove(0);
        }
    }
}

impl Drop for ProductionCores {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(windows)]
fn resolve_client(
    client: &ClientIdentity,
    host: &Path,
    generation: u64,
) -> Result<ResolvedRuntime, ResolveError> {
    let sid = client
        .user_key
        .split_once(':')
        .map(|(_, sid)| sid)
        .ok_or(ResolveError::Owner)?;
    // The daemon keeps its data root owner-only, so SYSTEM reads it with SeBackupPrivilege
    // (the Linux unit's CAP_DAC_READ_SEARCH); the resolver's checks decide what is accepted.
    let resolved = crate::runtime_paths::with_backup_privilege(|| {
        crate::runtime_paths::resolve_from(&client.data_root, host, generation)
    })?;
    // The data root is the client's own (redirectable) LocalAppData: refuse files that belong
    // to another user.
    resolved.verify_owned_by(&client.data_root, sid)?;
    Ok(resolved)
}

/// Re-opens every guarded path (with the same access as [`resolve_client`]) and compares
/// identities.
#[cfg(windows)]
fn verify_unchanged(resolved: &ResolvedRuntime) -> Result<(), ResolveError> {
    crate::runtime_paths::with_backup_privilege(|| resolved.verify_unchanged())
}

#[cfg(unix)]
fn verify_unchanged(resolved: &ResolvedRuntime) -> Result<(), ResolveError> {
    resolved.verify_unchanged()
}

#[cfg(unix)]
fn resolve_client(
    client: &ClientIdentity,
    host: &Path,
    generation: u64,
) -> Result<ResolvedRuntime, ResolveError> {
    crate::unix_runtime_paths::resolve_from(&client.data_root, host, generation, client.uid)
}

/// Reads a staged config through the resolver's verified handle, never by re-opening its path.
fn read_guarded(
    resolved: &ResolvedRuntime,
    path: &Path,
) -> Result<Vec<u8>, (BrokerErrorCode, ResolveError)> {
    resolved
        .read_guarded(path, MAX_CONFIG_BYTES as u64)
        .map_err(|error| match error {
            ResolveError::Layout => (BrokerErrorCode::InvalidRequest, error),
            _ => (BrokerErrorCode::NotReady, error),
        })
}

/// The client's own Mihomo home (`-d`, `SAFE_PATHS`): provider caches, `cache.db` and geodata
/// are never shared between users. Keyed by the stable account identity (Windows SID, Unix uid),
/// not the logon session, so a user keeps their caches across logons.
pub(crate) fn client_home(service_directory: &Path, client: &ClientIdentity) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(stable_identity(client).as_bytes());
    let name = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    service_directory.join("users").join(name)
}

/// Windows `user_key` is `"<session>:<sid>"`: the SID part. Unix: the uid.
pub(crate) fn stable_identity(client: &ClientIdentity) -> String {
    if cfg!(windows) {
        let sid = client
            .user_key
            .split_once(':')
            .map_or(client.user_key.as_str(), |(_, sid)| sid);
        format!("sid:{sid}")
    } else {
        format!("uid:{}", client.uid)
    }
}

fn free_loopback_port() -> Option<u16> {
    TcpListener::bind(("127.0.0.1", 0))
        .ok()?
        .local_addr()
        .ok()
        .map(|address| address.port())
}

fn random_secret() -> Result<Zeroizing<String>, getrandom::Error> {
    let mut bytes = Zeroizing::new([0_u8; 32]);
    getrandom::fill(bytes.as_mut())?;
    Ok(Zeroizing::new(
        bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
    ))
}

fn start_error(error: &ProcessError) -> BrokerErrorCode {
    match error {
        ProcessError::ReadinessTimedOut(_) => BrokerErrorCode::TimedOut,
        _ => BrokerErrorCode::NotReady,
    }
}
