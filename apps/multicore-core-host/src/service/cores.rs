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
use super::{mihomo_rest, state_dir};
use crate::runtime_host::{broker_safe_log, state};

#[cfg(windows)]
use crate::runtime_paths::ResolvedRuntime;
#[cfg(unix)]
use crate::unix_runtime_paths::ResolvedRuntime;
#[cfg(unix)]
use multicore_core::UnixGroupLauncher as PlatformLauncher;
#[cfg(windows)]
use multicore_core::WindowsJobLauncher as PlatformLauncher;

/// How long `SetTun{on:true}` waits for the `MultiCore` adapter and Mihomo to agree.
pub(crate) const TUN_READY_TIMEOUT: Duration = Duration::from_secs(15);
const TUN_POLL: Duration = Duration::from_millis(100);

struct ActiveCores {
    /// What the daemon gets: the proxy's port and session token, and the mixed port (0 for a
    /// TUN generation, which opens none).
    endpoint: ControllerEndpoint,
    /// Mihomo's own controller; never leaves the service.
    controller_port: u16,
    controller_secret: Zeroizing<String>,
    _proxy: ControllerProxy,
    controller: SidecarProcessController<PlatformLauncher>,
    _validated: ResolvedRuntime,
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
        let resolved = resolve_client(client, &self.host_executable, generation)
            .map_err(|()| BrokerErrorCode::NotReady)?;
        #[cfg(windows)]
        {
            let package = self
                .host_executable
                .parent()
                .and_then(Path::parent)
                .ok_or(BrokerErrorCode::NotReady)?;
            super::core_hashes::verify_core_hashes(
                package,
                &["cores/xray.exe", "cores/mihomo.exe"],
            )
            .map_err(|_| BrokerErrorCode::NotReady)?;
        }
        resolved
            .verify_unchanged()
            .map_err(|_| BrokerErrorCode::NotReady)?;
        let mihomo_source = read_bounded(&resolved.paths.mihomo_config)?;
        let xray_source = read_bounded(&resolved.paths.xray_config)?;
        let controller_port = free_loopback_port().ok_or(BrokerErrorCode::NotReady)?;
        let controller_secret = random_secret().map_err(|_| BrokerErrorCode::NotReady)?;
        let mihomo = sanitize_mihomo(
            &mihomo_source,
            &MihomoServicePolicy {
                service_directory: &self.service_directory,
                controller_port,
                controller_secret: &controller_secret,
            },
        )
        .map_err(|_| BrokerErrorCode::InvalidRequest)?;
        let xray = sanitize_xray(&xray_source).map_err(|_| BrokerErrorCode::InvalidRequest)?;
        let directory = state_dir::fresh_generation_directory(&self.service_directory, generation)
            .map_err(|_| BrokerErrorCode::NotReady)?;
        for sub in ["providers", "logs"] {
            fs::create_dir_all(self.service_directory.join(sub))
                .map_err(|_| BrokerErrorCode::NotReady)?;
        }
        fs::write(directory.join("mihomo.yaml"), &mihomo.yaml)
            .and_then(|()| fs::write(directory.join("xray.json"), &xray))
            .map_err(|_| BrokerErrorCode::NotReady)?;
        let paths = RuntimePaths {
            xray_binary: resolved.paths.xray_binary.clone(),
            mihomo_binary: resolved.paths.mihomo_binary.clone(),
            xray_config: directory.join("xray.json"),
            mihomo_config: directory.join("mihomo.yaml"),
            dashboard_directory: None,
            mihomo_home_directory: Some(self.service_directory.clone()),
        };
        let logs = Arc::new(CoreLogBuffer::persistent_or_memory(
            self.service_directory.join("logs").join("latest-core.log"),
        ));
        let launcher = PlatformLauncher::for_runtime(paths.clone(), logs, CORE_READINESS_TIMEOUT)
            .map_err(|_| BrokerErrorCode::NotReady)?;
        let controller = SidecarProcessController::with_launcher(paths, launcher);
        for engine in [Engine::Xray, Engine::Mihomo] {
            if let Err(error) = self.runtime.block_on(controller.start(engine)) {
                self.stop_processes(&controller);
                return Err(start_error(error));
            }
        }
        let token = match random_secret() {
            Ok(token) => token,
            Err(_) => {
                self.stop_processes(&controller);
                return Err(BrokerErrorCode::NotReady);
            }
        };
        let proxy = match self.runtime.block_on(ControllerProxy::start(
            controller_port,
            controller_secret.as_str().to_owned(),
            token.as_str().to_owned(),
        )) {
            Ok(proxy) => proxy,
            Err(_) => {
                self.stop_processes(&controller);
                return Err(BrokerErrorCode::NotReady);
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
            _validated: resolved,
        });
        Ok(endpoint)
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
        for engine in [Engine::Xray, Engine::Mihomo] {
            self.runtime
                .block_on(active.controller.start(engine))
                .map_err(start_error)?;
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
) -> Result<ResolvedRuntime, ()> {
    crate::runtime_paths::resolve_from(&client.data_root, host, generation).map_err(drop)
}

#[cfg(unix)]
fn resolve_client(
    client: &ClientIdentity,
    host: &Path,
    generation: u64,
) -> Result<ResolvedRuntime, ()> {
    crate::unix_runtime_paths::resolve_from(&client.data_root, host, generation, client.uid)
        .map_err(drop)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, BrokerErrorCode> {
    use std::io::Read;
    let limit = MAX_CONFIG_BYTES as u64;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|_| BrokerErrorCode::NotReady)?;
    if bytes.len() as u64 > limit {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    Ok(bytes)
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

fn start_error(error: ProcessError) -> BrokerErrorCode {
    match error {
        ProcessError::ReadinessTimedOut(_) => BrokerErrorCode::TimedOut,
        _ => BrokerErrorCode::NotReady,
    }
}
