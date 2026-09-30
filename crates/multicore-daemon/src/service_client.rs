//! The always-ready service seen from the daemon: one persistent channel, the controller
//! endpoint it hands out, and `ProcessController`s that turn Connect / Disconnect into
//! `Prepare` + `SetTun` instead of starting and stopping processes.

use std::{
    fmt,
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use multicore_core::{
    CORE_READINESS_TIMEOUT, CoreLogRecord, DiagnosticStream, Engine, ProcessController,
    ProcessDiagnostics, RuntimeCheckState, Snapshot, StartFailureCause,
    elevation_protocol::{
        BrokerErrorCode, ElevatedDiagnosticStream, ElevatedEngine, ElevatedRuntimeState,
    },
    service_protocol::{ServiceCommand, ServiceResponse},
};
use sha2::{Digest, Sha256};

use crate::{LatencyProbeError, MihomoHttpSelector, MihomoSelector, SelectorError, TrafficTotals};

/// Where the current Mihomo controller listens. The secret never leaves this process except
/// towards Mihomo itself.
#[derive(Clone)]
pub struct ControllerTarget {
    pub address: SocketAddr,
    secret: Arc<str>,
    pub mixed_port: Option<u16>,
}

impl ControllerTarget {
    #[must_use]
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for ControllerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControllerTarget")
            .field("address", &self.address)
            .field("secret", &"[REDACTED]")
            .field("mixed_port", &self.mixed_port)
            .finish()
    }
}

/// The controller endpoint shared by the selector, the dashboard proxy and the service
/// session. On-demand mode sets it once; the service replaces it on every `Prepare`.
#[derive(Default)]
pub struct ControllerSlot {
    target: RwLock<Option<ControllerTarget>>,
    version: AtomicU64,
}

impl ControllerSlot {
    pub fn set(&self, address: SocketAddr, secret: &str, mixed_port: Option<u16>) {
        if let Ok(mut target) = self.target.write() {
            *target = Some(ControllerTarget {
                address,
                secret: Arc::from(secret),
                mixed_port,
            });
        }
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    #[must_use]
    pub fn get(&self) -> Option<ControllerTarget> {
        self.target.read().ok().and_then(|target| target.clone())
    }

    #[must_use]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// A `MihomoSelector` that always talks to the slot's current controller.
pub struct SlotSelector {
    slot: Arc<ControllerSlot>,
    cached: Mutex<Option<(u64, Arc<MihomoHttpSelector>)>>,
}

impl SlotSelector {
    #[must_use]
    pub fn new(slot: Arc<ControllerSlot>) -> Self {
        Self {
            slot,
            cached: Mutex::new(None),
        }
    }

    fn current(&self) -> Option<Arc<MihomoHttpSelector>> {
        let version = self.slot.version();
        let mut cached = self.cached.lock().ok()?;
        if let Some((cached_version, selector)) = cached.as_ref()
            && *cached_version == version
        {
            return Some(selector.clone());
        }
        let target = self.slot.get()?;
        let selector = Arc::new(MihomoHttpSelector::new(target.address, target.secret()).ok()?);
        *cached = Some((version, selector.clone()));
        Some(selector)
    }
}

#[async_trait]
impl MihomoSelector for SlotSelector {
    async fn select(&self, raw_group: &str, raw_node: &str) -> Result<(), SelectorError> {
        let selector = self.current().ok_or_else(SelectorError::new)?;
        selector.select(raw_group, raw_node).await
    }

    async fn probe_latency(&self, raw_name: &str) -> Result<u64, LatencyProbeError> {
        let selector = self.current().ok_or(LatencyProbeError::Unavailable)?;
        selector.probe_latency(raw_name).await
    }

    async fn traffic_totals(&self) -> Result<TrafficTotals, SelectorError> {
        let selector = self.current().ok_or_else(SelectorError::new)?;
        selector.traffic_totals().await
    }

    async fn proxy_now(&self, raw_name: &str) -> Result<Option<String>, SelectorError> {
        let selector = self.current().ok_or_else(SelectorError::new)?;
        selector.proxy_now(raw_name).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceError {
    /// No MultiCore service is registered on this machine.
    NotInstalled,
    /// The service is registered but not reachable right now.
    Unavailable,
    /// Something other than the registered service owns the channel.
    Impostor,
    /// Another interactive user's session holds the cores.
    Busy,
    Disconnected,
    TimedOut,
    Protocol,
    Rejected(BrokerErrorCode),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "MultiCore service: {self:?}")
    }
}

impl std::error::Error for ServiceError {}

impl ServiceError {
    #[must_use]
    pub fn start_failure_cause(self) -> StartFailureCause {
        match self {
            Self::Busy => StartFailureCause::ServiceBusy,
            Self::Rejected(BrokerErrorCode::TimedOut) => StartFailureCause::ReadinessTimedOut,
            Self::Rejected(_) => StartFailureCause::CoreFailed,
            _ => StartFailureCause::BrokerFailed,
        }
    }
}

pub trait ServiceTransport: Send {
    fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError>;
}

pub trait ServiceConnector: Send + Sync + 'static {
    type Transport: ServiceTransport;
    fn connect(&self) -> Result<Self::Transport, ServiceError>;
}

/// How long to wait for the service's answer to `command`.
#[must_use]
pub fn service_response_timeout(command: &ServiceCommand) -> Duration {
    match command {
        ServiceCommand::Prepare { .. } => CORE_READINESS_TIMEOUT * 2 + Duration::from_secs(10),
        ServiceCommand::SetTun { .. } => Duration::from_secs(25),
        ServiceCommand::Update { .. } => Duration::from_secs(15 * 60),
        _ => Duration::from_secs(5),
    }
}

pub struct ServiceSession<C: ServiceConnector> {
    connector: C,
    transport: Mutex<Option<C::Transport>>,
    prepared: Mutex<Option<u64>>,
    healthy: AtomicBool,
    controller: Arc<ControllerSlot>,
}

impl<C: ServiceConnector> ServiceSession<C> {
    pub fn new(connector: C, controller: Arc<ControllerSlot>) -> Self {
        Self {
            connector,
            transport: Mutex::new(None),
            prepared: Mutex::new(None),
            healthy: AtomicBool::new(false),
            controller,
        }
    }

    fn prepared_generation(&self) -> Option<u64> {
        self.prepared.lock().ok().and_then(|prepared| *prepared)
    }

    fn forget(&self) {
        if let Ok(mut prepared) = self.prepared.lock() {
            *prepared = None;
        }
        self.healthy.store(false, Ordering::Release);
    }

    /// Sends `command`, connecting first when `connect` allows it. A failed exchange drops the
    /// channel; the service then stops the cores, so the prepared generation is forgotten.
    fn request(
        &self,
        command: &ServiceCommand,
        connect: bool,
    ) -> Result<ServiceResponse, ServiceError> {
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| ServiceError::Disconnected)?;
        if transport.is_none() {
            if !connect {
                return Err(ServiceError::Disconnected);
            }
            self.forget();
            *transport = Some(self.connector.connect()?);
        }
        let result = transport
            .as_ref()
            .expect("service transport exists")
            .request(command);
        if result.is_err() {
            transport.take();
            self.forget();
        }
        match result? {
            ServiceResponse::Busy => Err(ServiceError::Busy),
            ServiceResponse::Error { code } => Err(ServiceError::Rejected(code)),
            response => Ok(response),
        }
    }

    pub fn probe(&self) -> Result<(), ServiceError> {
        match self.request(&ServiceCommand::Status, true)? {
            ServiceResponse::Status { .. } => Ok(()),
            _ => Err(ServiceError::Protocol),
        }
    }

    pub fn prepare(&self, generation: u64) -> Result<(), ServiceError> {
        if self.prepared_generation() == Some(generation) && self.healthy.load(Ordering::Acquire) {
            return Ok(());
        }
        match self.request(
            &ServiceCommand::Prepare {
                generation_id: generation,
            },
            true,
        )? {
            ServiceResponse::Prepared {
                generation_id,
                controller,
            } if generation_id == generation => {
                self.controller.set(
                    SocketAddr::from((Ipv4Addr::LOCALHOST, controller.port)),
                    &controller.secret,
                    Some(controller.mixed_port),
                );
                if let Ok(mut prepared) = self.prepared.lock() {
                    *prepared = Some(generation);
                }
                self.healthy.store(true, Ordering::Release);
                Ok(())
            }
            _ => Err(ServiceError::Protocol),
        }
    }

    pub fn set_tun(&self, on: bool) -> Result<(), ServiceError> {
        match self.request(&ServiceCommand::SetTun { on }, false)? {
            ServiceResponse::TunState { on: actual } if actual == on => Ok(()),
            _ => Err(ServiceError::Protocol),
        }
    }

    /// Refreshes readiness from `Status` over an existing channel (never connects).
    pub fn poll(&self) {
        let healthy = match self.request(&ServiceCommand::Status, false) {
            Ok(ServiceResponse::Status {
                generation_id,
                xray,
                mihomo,
                failure,
                ..
            }) => {
                failure.is_none()
                    && xray == ElevatedRuntimeState::Ready
                    && mihomo == ElevatedRuntimeState::Ready
                    && generation_id.is_some()
                    && generation_id == self.prepared_generation()
            }
            _ => false,
        };
        self.healthy.store(healthy, Ordering::Release);
    }

    #[must_use]
    pub fn cores_ready(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && self.prepared_generation().is_some()
    }

    pub fn shutdown(&self) {
        if let Ok(mut transport) = self.transport.lock()
            && let Some(transport) = transport.take()
        {
            let _ = transport.request(&ServiceCommand::Shutdown);
        }
        self.forget();
    }

    pub fn update(&self, version: &str) -> Result<(), ServiceError> {
        match self.request(
            &ServiceCommand::Update {
                version: version.to_owned(),
            },
            true,
        )? {
            ServiceResponse::UpdateStarted => Ok(()),
            _ => Err(ServiceError::Protocol),
        }
    }

    #[must_use]
    pub fn diagnostics(&self) -> ProcessDiagnostics {
        match self.request(&ServiceCommand::Diagnostics, false) {
            Ok(ServiceResponse::ProcessDiagnostics {
                xray,
                mihomo,
                tun,
                logs,
            }) => ProcessDiagnostics {
                xray: check_state(xray),
                mihomo: check_state(mihomo),
                tun: check_state(tun),
                logs: logs
                    .into_iter()
                    .map(|record| CoreLogRecord {
                        id: record.id,
                        timestamp_ms: record.timestamp_ms,
                        engine: match record.engine {
                            ElevatedEngine::Xray => Engine::Xray,
                            ElevatedEngine::Mihomo => Engine::Mihomo,
                        },
                        stream: match record.stream {
                            ElevatedDiagnosticStream::Stdout => DiagnosticStream::Stdout,
                            ElevatedDiagnosticStream::Stderr => DiagnosticStream::Stderr,
                        },
                        message: record.message,
                    })
                    .collect(),
            },
            _ => ProcessDiagnostics::default(),
        }
    }
}

fn check_state(state: ElevatedRuntimeState) -> RuntimeCheckState {
    match state {
        ElevatedRuntimeState::Stopped => RuntimeCheckState::Stopped,
        ElevatedRuntimeState::Starting => RuntimeCheckState::Starting,
        ElevatedRuntimeState::Ready => RuntimeCheckState::Ready,
        ElevatedRuntimeState::Failed => RuntimeCheckState::Failed,
        ElevatedRuntimeState::Unsupported => RuntimeCheckState::Unsupported,
    }
}

async fn blocking<C: ServiceConnector, T: Send + 'static>(
    session: &Arc<ServiceSession<C>>,
    work: impl FnOnce(&ServiceSession<C>) -> T + Send + 'static,
) -> Result<T, ServiceError> {
    let session = session.clone();
    tokio::task::spawn_blocking(move || work(&session))
        .await
        .map_err(|_| ServiceError::Disconnected)
}

/// TUN mode through the service: `start(Xray)` prepares, `start(Mihomo)` switches TUN on,
/// `stop(Mihomo)` switches it off, `stop(Xray)` leaves the cores running.
pub struct ServiceTunController<C: ServiceConnector> {
    session: Arc<ServiceSession<C>>,
    generation: u64,
}

impl<C: ServiceConnector> ServiceTunController<C> {
    pub fn new(session: Arc<ServiceSession<C>>, generation: u64) -> Self {
        Self {
            session,
            generation,
        }
    }
}

impl<C: ServiceConnector> ProcessController for ServiceTunController<C> {
    type Error = ServiceError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        let generation = self.generation;
        Box::pin(async move {
            blocking(&self.session, move |session| match engine {
                Engine::Xray => session.prepare(generation),
                Engine::Mihomo => session.set_tun(true),
            })
            .await?
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Xray {
                return Ok(());
            }
            match blocking(&self.session, |session| session.set_tun(false)).await? {
                // A dropped channel already made the service stop the cores and the tunnel.
                Ok(()) | Err(ServiceError::Disconnected) => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            blocking(&self.session, |session| session.diagnostics())
                .await
                .unwrap_or_default()
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        error.start_failure_cause()
    }
}

/// System-proxy mode through the service: both starts only make sure the generation runs
/// with TUN off; stopping never stops the cores (the proxy switch is the daemon's job).
pub struct ServiceCoresController<C: ServiceConnector> {
    session: Arc<ServiceSession<C>>,
    generation: u64,
}

impl<C: ServiceConnector> ServiceCoresController<C> {
    pub fn new(session: Arc<ServiceSession<C>>, generation: u64) -> Self {
        Self {
            session,
            generation,
        }
    }
}

impl<C: ServiceConnector> ProcessController for ServiceCoresController<C> {
    type Error = ServiceError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        let generation = self.generation;
        Box::pin(async move {
            blocking(&self.session, move |session| {
                session.prepare(generation)?;
                if engine == Engine::Mihomo {
                    session.set_tun(false)?;
                }
                Ok(())
            })
            .await?
        })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            blocking(&self.session, |session| session.diagnostics())
                .await
                .unwrap_or_default()
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        error.start_failure_cause()
    }
}

/// Identity of a warm generation: the subscription payloads and the physical interface Xray
/// is bound to. A changed default interface (Wi-Fi ↔ Ethernet) must re-stage before Connect.
#[must_use]
pub fn warm_key(snapshot: &Snapshot, outbound_interface: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in [
        snapshot.mihomo.raw_yaml().as_bytes(),
        snapshot.xray.raw_bytes(),
        outbound_interface.as_bytes(),
    ] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part);
    }
    digest.finalize().into()
}

#[cfg(windows)]
pub mod windows {
    use super::*;
    use crate::elevated_controller::{
        ElevationError,
        windows::{read_message, write_raw_message},
    };
    use multicore_core::service_protocol::{
        decode_service_frame, encode_service_frame, service_response_matches,
    };
    use std::{
        ffi::OsStr,
        fs::OpenOptions,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_SERVICE_DOES_NOT_EXIST, GetLastError},
        Storage::FileSystem::{
            FILE_FLAG_OVERLAPPED, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
        },
        System::{
            Pipes::{
                GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, SetNamedPipeHandleState,
                WaitNamedPipeW,
            },
            Services::{
                CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
                SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
                SERVICE_STATUS_PROCESS,
            },
        },
    };

    pub const SERVICE_PIPE_NAME: &str = r"\\.\pipe\MultiCore.Service.v2";
    pub const SERVICE_NAME: &str = "MultiCore";

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    fn map(error: ElevationError) -> ServiceError {
        match error {
            ElevationError::TimedOut => ServiceError::TimedOut,
            ElevationError::Protocol => ServiceError::Protocol,
            _ => ServiceError::Disconnected,
        }
    }

    pub struct PipeTransport {
        pipe: std::fs::File,
    }

    impl ServiceTransport for PipeTransport {
        fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError> {
            let cancelled = AtomicBool::new(false);
            let frame = encode_service_frame(command).map_err(|_| ServiceError::Protocol)?;
            write_raw_message(
                self.pipe.as_raw_handle(),
                frame,
                Duration::from_secs(5),
                &cancelled,
            )
            .map_err(map)?;
            let reply = read_message(
                self.pipe.as_raw_handle(),
                service_response_timeout(command),
                &cancelled,
            )
            .map_err(map)?;
            let response: ServiceResponse =
                decode_service_frame(&reply).map_err(|_| ServiceError::Protocol)?;
            if !service_response_matches(command, &response) {
                return Err(ServiceError::Protocol);
            }
            Ok(response)
        }
    }

    /// PID of the running `MultiCore` service as the SCM knows it.
    pub fn registered_service_pid() -> Result<u32, ServiceError> {
        let manager = unsafe { OpenSCManagerW(ptr::null(), ptr::null(), SC_MANAGER_CONNECT) };
        if manager.is_null() {
            return Err(ServiceError::Unavailable);
        }
        let name = wide(SERVICE_NAME);
        let service = unsafe { OpenServiceW(manager, name.as_ptr(), SERVICE_QUERY_STATUS) };
        if service.is_null() {
            let error = unsafe { GetLastError() };
            unsafe { CloseServiceHandle(manager) };
            return Err(if error == ERROR_SERVICE_DOES_NOT_EXIST {
                ServiceError::NotInstalled
            } else {
                ServiceError::Unavailable
            });
        }
        let mut status: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
        let mut needed = 0;
        let queried = unsafe {
            QueryServiceStatusEx(
                service,
                SC_STATUS_PROCESS_INFO,
                (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            )
        };
        unsafe {
            CloseServiceHandle(service);
            CloseServiceHandle(manager);
        }
        if queried == 0 || status.dwCurrentState != SERVICE_RUNNING || status.dwProcessId == 0 {
            return Err(ServiceError::Unavailable);
        }
        Ok(status.dwProcessId)
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct WindowsServiceConnector;

    impl ServiceConnector for WindowsServiceConnector {
        type Transport = PipeTransport;

        fn connect(&self) -> Result<PipeTransport, ServiceError> {
            let service_pid = registered_service_pid()?;
            let name = wide(SERVICE_PIPE_NAME);
            if unsafe { WaitNamedPipeW(name.as_ptr(), 2_000) } == 0 {
                return Err(ServiceError::Unavailable);
            }
            let pipe = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                )
                .open(SERVICE_PIPE_NAME)
                .map_err(|_| ServiceError::Unavailable)?;
            let mode = PIPE_READMODE_MESSAGE;
            if unsafe {
                SetNamedPipeHandleState(pipe.as_raw_handle(), &mode, ptr::null(), ptr::null())
            } == 0
            {
                return Err(ServiceError::Unavailable);
            }
            let mut server_pid = 0;
            if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server_pid) } == 0
                || server_pid != service_pid
            {
                return Err(ServiceError::Impostor);
            }
            Ok(PipeTransport { pipe })
        }
    }
}

#[cfg(unix)]
pub mod unix {
    use super::*;
    use multicore_core::{
        elevation_protocol::read_stream_frame,
        service_protocol::{decode_service_frame, service_response_matches, write_service_frame},
    };
    use std::{os::unix::net::UnixStream, path::Path};

    pub const SOCKET_PATH: &str = "/run/multicore/service.sock";
    const UNIT_PATHS: [&str; 2] = [
        "/usr/lib/systemd/system/multicore.service",
        "/lib/systemd/system/multicore.service",
    ];

    pub struct SocketTransport {
        stream: UnixStream,
    }

    impl ServiceTransport for SocketTransport {
        fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError> {
            let mut stream = &self.stream;
            self.stream
                .set_read_timeout(Some(service_response_timeout(command)))
                .map_err(|_| ServiceError::Disconnected)?;
            write_service_frame(&mut stream, command).map_err(|_| ServiceError::Disconnected)?;
            let frame = read_stream_frame(&mut stream).map_err(|error| {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) {
                    ServiceError::TimedOut
                } else {
                    ServiceError::Disconnected
                }
            })?;
            let response: ServiceResponse =
                decode_service_frame(&frame).map_err(|_| ServiceError::Protocol)?;
            if !service_response_matches(command, &response) {
                return Err(ServiceError::Protocol);
            }
            Ok(response)
        }
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct UnixServiceConnector;

    impl ServiceConnector for UnixServiceConnector {
        type Transport = SocketTransport;

        fn connect(&self) -> Result<SocketTransport, ServiceError> {
            if !Path::new(SOCKET_PATH).exists() {
                return Err(if UNIT_PATHS.iter().any(|unit| Path::new(unit).exists()) {
                    ServiceError::Unavailable
                } else {
                    ServiceError::NotInstalled
                });
            }
            let stream = UnixStream::connect(SOCKET_PATH).map_err(|_| ServiceError::Unavailable)?;
            let peer = rustix::net::sockopt::socket_peercred(&stream)
                .map_err(|_| ServiceError::Unavailable)?;
            if peer.uid.as_raw() != 0 {
                return Err(ServiceError::Impostor);
            }
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .map_err(|_| ServiceError::Unavailable)?;
            Ok(SocketTransport { stream })
        }
    }
}

#[cfg(windows)]
pub type ProductionServiceConnector = windows::WindowsServiceConnector;
#[cfg(unix)]
pub type ProductionServiceConnector = unix::UnixServiceConnector;
