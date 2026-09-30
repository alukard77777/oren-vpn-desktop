use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::TcpStream,
    process::Child,
    sync::Mutex,
    time::{Duration, Instant, sleep},
};

use crate::{Engine, ProcessController};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreLogRecord {
    pub id: u64,
    pub timestamp_ms: u64,
    pub engine: Engine,
    pub stream: DiagnosticStream,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeCheckState {
    #[default]
    Stopped,
    Starting,
    Ready,
    Failed,
    Unsupported,
    /// Deliberately off: TUN in system-proxy mode.
    Disabled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessDiagnostics {
    pub xray: RuntimeCheckState,
    pub mihomo: RuntimeCheckState,
    pub tun: RuntimeCheckState,
    pub logs: Vec<CoreLogRecord>,
}

#[derive(Debug)]
struct CoreLogState {
    records: VecDeque<CoreLogRecord>,
}

#[derive(Debug)]
pub struct CoreLogBuffer {
    state: StdMutex<CoreLogState>,
    persistent: Option<StdMutex<PersistentLogSink>>,
    next_id: AtomicU64,
    max_records: usize,
    max_chars: usize,
}

/// Size at which the persistent core log rotates. At most two files exist: the log itself and
/// one rotated predecessor (`<name>.1`), so the log never uses more than twice this on disk.
pub const MAX_PERSISTENT_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// The rotated predecessor of a persistent core log: `latest-core.log` -> `latest-core.log.1`.
pub fn rotated_log_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

#[derive(Debug)]
struct PersistentLogSink {
    path: PathBuf,
    file: File,
    bytes_written: u64,
    disabled: bool,
}

impl PersistentLogSink {
    fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            bytes_written: 0,
            disabled: false,
        })
    }

    /// Moves the full log to `<name>.1` (replacing the previous one) and starts an empty log;
    /// if the rename is impossible the log is truncated in place instead.
    fn rotate(&mut self) -> std::io::Result<()> {
        if fs::rename(&self.path, rotated_log_path(&self.path)).is_ok() {
            self.file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        } else {
            self.file.set_len(0)?;
            self.file.seek(SeekFrom::Start(0))?;
        }
        self.bytes_written = 0;
        Ok(())
    }

    fn write(&mut self, record: &CoreLogRecord) {
        if self.disabled {
            return;
        }
        let engine = match record.engine {
            Engine::Xray => "xray",
            Engine::Mihomo => "mihomo",
        };
        let stream = match record.stream {
            DiagnosticStream::Stdout => "stdout",
            DiagnosticStream::Stderr => "stderr",
        };
        let message: String = record
            .message
            .chars()
            .map(|character| match character {
                '\r' | '\n' | '\t' => ' ',
                other => other,
            })
            .collect();
        let line = format!("{}\t{engine}\t{stream}\t{message}\n", record.timestamp_ms);
        let bytes = line.as_bytes();
        let result = (|| -> std::io::Result<()> {
            if self.bytes_written.saturating_add(bytes.len() as u64) > MAX_PERSISTENT_LOG_BYTES {
                self.rotate()?;
            }
            self.file.write_all(bytes)?;
            self.file.flush()?;
            self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
            Ok(())
        })();
        if result.is_err() {
            self.disabled = true;
        }
    }
}

impl Default for CoreLogBuffer {
    fn default() -> Self {
        Self::with_limits(256, 2_048)
    }
}

impl CoreLogBuffer {
    pub fn with_limits(max_records: usize, max_chars: usize) -> Self {
        Self {
            state: StdMutex::new(CoreLogState {
                records: VecDeque::with_capacity(max_records),
            }),
            persistent: None,
            next_id: AtomicU64::new(1),
            max_records: max_records.max(1),
            max_chars: max_chars.max(1),
        }
    }

    pub fn persistent(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            state: StdMutex::new(CoreLogState {
                records: VecDeque::with_capacity(256),
            }),
            persistent: Some(StdMutex::new(PersistentLogSink::create(path.as_ref())?)),
            next_id: AtomicU64::new(1),
            max_records: 256,
            max_chars: 2_048,
        })
    }

    pub fn persistent_or_memory(path: impl AsRef<Path>) -> Self {
        Self::persistent(path).unwrap_or_default()
    }

    pub fn push(&self, engine: Engine, stream: DiagnosticStream, message: &str) {
        let safe = crate::event::redact_text(message);
        let message = truncate_chars(&safe, self.max_chars);
        if message.trim().is_empty() {
            return;
        }
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let record = CoreLogRecord {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            timestamp_ms,
            engine,
            stream,
            message,
        };
        let mut state = self.state.lock().expect("core log buffer lock poisoned");
        while state.records.len() >= self.max_records {
            state.records.pop_front();
        }
        state.records.push_back(record.clone());
        drop(state);
        if let Some(persistent) = &self.persistent {
            persistent
                .lock()
                .expect("persistent core log lock poisoned")
                .write(&record);
        }
    }

    pub fn snapshot(&self) -> Vec<CoreLogRecord> {
        self.state
            .lock()
            .expect("core log buffer lock poisoned")
            .records
            .iter()
            .cloned()
            .collect()
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value.chars().take(max_chars).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    pub xray_binary: PathBuf,
    pub mihomo_binary: PathBuf,
    pub xray_config: PathBuf,
    pub mihomo_config: PathBuf,
    /// The vendored, checksum-verified static dashboard directory (see
    /// `crates/multicore-core/src/snapshot.rs`'s `external-ui` overlay). When set, Mihomo is
    /// launched with `SAFE_PATHS` naming this exact directory, which is the only way to let
    /// Mihomo serve an `external-ui` directory that lies outside its own home directory without
    /// copying the dashboard into the ephemeral runtime tree on every Connect. Absent in builds
    /// without a vendored dashboard.
    pub dashboard_directory: Option<PathBuf>,
    /// Service mode: Mihomo's home directory (`-d`). Relative provider caches, `cache.db` and
    /// geodata live here, and it is the only `SAFE_PATHS` entry besides the dashboard. A service
    /// TUN generation opens no inbound port, so its readiness is the controller alone.
    pub mihomo_home_directory: Option<PathBuf>,
}

impl RuntimePaths {
    pub fn command(&self, engine: Engine) -> CommandSpec {
        match engine {
            Engine::Xray => CommandSpec {
                engine,
                program: self.xray_binary.clone(),
                args: vec![
                    OsString::from("run"),
                    OsString::from("-config"),
                    self.xray_config.clone().into_os_string(),
                ],
                env: Vec::new(),
            },
            Engine::Mihomo => {
                let mut args = Vec::new();
                if let Some(home) = &self.mihomo_home_directory {
                    args.push(OsString::from("-d"));
                    args.push(home.clone().into_os_string());
                }
                args.push(OsString::from("-f"));
                args.push(self.mihomo_config.clone().into_os_string());
                let safe_paths = self
                    .dashboard_directory
                    .iter()
                    .chain(self.mihomo_home_directory.iter())
                    .cloned()
                    .collect::<Vec<_>>();
                // A single entry is passed verbatim; only several need the OS list separator
                // (and one containing it cannot be listed, which leaves Mihomo fail-closed).
                let env = match safe_paths.as_slice() {
                    [] => Vec::new(),
                    [single] => vec![(
                        OsString::from("SAFE_PATHS"),
                        single.clone().into_os_string(),
                    )],
                    _ => std::env::join_paths(safe_paths)
                        .map(|joined| vec![(OsString::from("SAFE_PATHS"), joined)])
                        .unwrap_or_default(),
                };
                CommandSpec {
                    engine,
                    program: self.mihomo_binary.clone(),
                    args,
                    env,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub engine: Engine,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Additional environment variables the launcher must set on top of its own sanitized base
    /// environment (never used to remove or replace anything already set).
    pub env: Vec<(OsString, OsString)>,
}

/// Longest time a launched core may take to report readiness before its start is failed.
///
/// Callers that wait on a remote start (the daemon talking to the elevated host) must allow at
/// least this long plus launch overhead.
pub const CORE_READINESS_TIMEOUT: Duration = Duration::from_secs(12);

/// How often a readiness-wait loop re-checks a launched core.
///
/// Small enough that a core becoming ready between checks is reported with only brief latency,
/// not a busy loop that wastes CPU while a core takes its full startup time.
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProcessError {
    #[error("failed to launch {0:?}")]
    LaunchFailed(Engine),
    #[error("failed to inspect {0:?}")]
    InspectFailed(Engine),
    #[error("failed to stop {0:?}")]
    StopFailed(Engine),
    #[error("{0:?} did not become ready")]
    ReadinessFailed(Engine),
    #[error("{0:?} did not become ready in time")]
    ReadinessTimedOut(Engine),
}

pub trait ManagedChild: Send {
    fn has_exited(&mut self) -> Result<bool, ProcessError>;

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>>;
}

pub trait ProcessLauncher: Send + Sync {
    type Child: ManagedChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>>;

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if child.has_exited()? {
                Err(ProcessError::ReadinessFailed(command.engine))
            } else {
                Ok(())
            }
        })
    }

    fn current_ready<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async { true })
    }

    fn diagnostic_logs(&self) -> Vec<CoreLogRecord> {
        Vec::new()
    }

    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async { RuntimeCheckState::Unsupported })
    }
}

#[derive(Debug, Clone, Default)]
pub struct TokioProcessLauncher {
    logs: Arc<CoreLogBuffer>,
    runtime: Option<RuntimeReadiness>,
}

#[derive(Debug, Clone)]
struct RuntimeReadiness {
    paths: RuntimePaths,
    timeout: Duration,
}

impl TokioProcessLauncher {
    pub fn for_runtime(paths: RuntimePaths, timeout: Duration) -> Self {
        Self::for_runtime_with_logs(paths, timeout, Arc::default())
    }

    pub fn for_runtime_with_logs(
        paths: RuntimePaths,
        timeout: Duration,
        logs: Arc<CoreLogBuffer>,
    ) -> Self {
        Self {
            logs,
            runtime: Some(RuntimeReadiness { paths, timeout }),
        }
    }

    pub fn logs(&self) -> Arc<CoreLogBuffer> {
        self.logs.clone()
    }

    async fn runtime_ready(&self, engine: Engine) -> bool {
        let Some(runtime) = &self.runtime else {
            return true;
        };
        match engine {
            Engine::Xray => {
                let Ok(addresses) = xray_loopback_inbounds(&runtime.paths.xray_config) else {
                    return false;
                };
                if addresses.is_empty() {
                    // Only the idle config of a Mihomo-only subscription has
                    // no listener; any provider config must expose bridges.
                    return xray_config_is_idle(&runtime.paths.xray_config);
                }
                for address in addresses {
                    if TcpStream::connect(address).await.is_err() {
                        return false;
                    }
                }
                // The SOCKS ports accepting a raw TCP connection only proves
                // Xray's listeners are bound; it proves nothing about its DNS
                // client or the interface-bound outbound sockets it dials
                // through, which settle on their own timeline right after
                // this fresh process (and MultiCore's own brand-new TUN
                // adapter) starts. A live, real-hardware Connect showed a
                // domain-scoped endpoint DNS lookup that this port check
                // alone would have called "ready" for repeatedly fail with
                // "record not found" for several seconds right after
                // startup - live traffic (through Mihomo) was already
                // flowing into Xray during exactly that unsettled window.
                // Require one real proxied dial to succeed first, so nothing
                // downstream starts until Xray's actual DNS+dial path is
                // proven, not just its listeners.
                xray_domain_dns_warm(&runtime.paths.xray_config).await
            }
            Engine::Mihomo => {
                let Ok(targets) = mihomo_readiness_targets(&runtime.paths) else {
                    return false;
                };
                if TcpStream::connect(targets.controller).await.is_err() {
                    return false;
                }
                match targets.traffic {
                    MihomoTrafficEntry::Tun { device } => tun_device_is_up(&device).await,
                    // System-proxy mode: the proxy port accepting a connection is what
                    // applications will actually use.
                    MihomoTrafficEntry::MixedPort(address) => {
                        TcpStream::connect(address).await.is_ok()
                    }
                    MihomoTrafficEntry::ControllerOnly => true,
                }
            }
        }
    }
}

pub struct TokioManagedChild {
    engine: Engine,
    child: Child,
}

impl ManagedChild for TokioManagedChild {
    fn has_exited(&mut self) -> Result<bool, ProcessError> {
        self.child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|_| ProcessError::InspectFailed(self.engine))
    }

    fn stop<'a>(
        &'a mut self,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            if self.has_exited()? {
                return Ok(());
            }
            self.child
                .kill()
                .await
                .map_err(|_| ProcessError::StopFailed(self.engine))?;
            self.child
                .wait()
                .await
                .map_err(|_| ProcessError::StopFailed(self.engine))?;
            Ok(())
        })
    }
}

impl ProcessLauncher for TokioProcessLauncher {
    type Child = TokioManagedChild;

    fn launch<'a>(
        &'a self,
        command: CommandSpec,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Child, ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            let mut process = tokio::process::Command::new(&command.program);
            process
                .args(&command.args)
                .env_remove("MULTICORE_DAEMON_URL")
                .env_remove("MULTICORE_DAEMON_TOKEN")
                .env_remove("MULTICORE_DAEMON_READY_STDOUT")
                .env_remove("MULTICORE_DAEMON_ADDR")
                .env_remove("MULTICORE_DAEMON_DATA_DIR")
                .env_remove("MULTICORE_XRAY_BIN")
                .env_remove("MULTICORE_MIHOMO_BIN")
                .env_remove("MULTICORE_MIHOMO_CONTROLLER_ADDR")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let mut child = process
                .spawn()
                .map_err(|_| ProcessError::LaunchFailed(command.engine))?;
            if let Some(stdout) = child.stdout.take() {
                spawn_log_reader(
                    self.logs.clone(),
                    command.engine,
                    DiagnosticStream::Stdout,
                    stdout,
                );
            }
            if let Some(stderr) = child.stderr.take() {
                spawn_log_reader(
                    self.logs.clone(),
                    command.engine,
                    DiagnosticStream::Stderr,
                    stderr,
                );
            }
            Ok(TokioManagedChild {
                engine: command.engine,
                child,
            })
        })
    }

    fn wait_until_ready<'a>(
        &'a self,
        command: &'a CommandSpec,
        child: &'a mut Self::Child,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProcessError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(runtime) = &self.runtime else {
                return if child.has_exited()? {
                    Err(ProcessError::ReadinessFailed(command.engine))
                } else {
                    Ok(())
                };
            };
            let deadline = Instant::now() + runtime.timeout;
            loop {
                if child.has_exited()? {
                    return Err(ProcessError::ReadinessFailed(command.engine));
                }
                if self.runtime_ready(command.engine).await {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(ProcessError::ReadinessTimedOut(command.engine));
                }
                sleep(READINESS_POLL_INTERVAL).await;
            }
        })
    }

    fn current_ready<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move { self.runtime_ready(engine).await })
    }

    fn diagnostic_logs(&self) -> Vec<CoreLogRecord> {
        self.logs.snapshot()
    }

    fn tun_check<'a>(&'a self) -> Pin<Box<dyn Future<Output = RuntimeCheckState> + Send + 'a>> {
        Box::pin(async move {
            let Some(runtime) = &self.runtime else {
                return RuntimeCheckState::Unsupported;
            };
            let Ok(targets) = mihomo_readiness_targets(&runtime.paths) else {
                return RuntimeCheckState::Failed;
            };
            let MihomoTrafficEntry::Tun { device } = targets.traffic else {
                return RuntimeCheckState::Disabled;
            };
            if tun_device_is_up(&device).await {
                RuntimeCheckState::Ready
            } else {
                RuntimeCheckState::Failed
            }
        })
    }
}

fn spawn_log_reader<R>(
    logs: Arc<CoreLogBuffer>,
    engine: Engine,
    stream: DiagnosticStream,
    reader: R,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = reader;
        let mut chunk = [0_u8; 4096];
        let mut decoder = BoundedLogDecoder::default();
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => {
                    decoder.finish(|line| logs.push(engine, stream, line));
                    break;
                }
                Ok(read) => decoder.feed(&chunk[..read], |line| logs.push(engine, stream, line)),
                Err(_) => {
                    logs.push(
                        engine,
                        DiagnosticStream::Stderr,
                        "core log stream closed unexpectedly",
                    );
                    break;
                }
            }
        }
    });
}

const MAX_CAPTURED_LOG_LINE_BYTES: usize = 4096;

pub(crate) fn spawn_blocking_log_reader(
    logs: Arc<CoreLogBuffer>,
    engine: Engine,
    stream: DiagnosticStream,
    mut reader: impl std::io::Read + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        let mut decoder = BoundedLogDecoder::default();
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    decoder.finish(|line| logs.push(engine, stream, line));
                    break;
                }
                Ok(read) => decoder.feed(&chunk[..read], |line| logs.push(engine, stream, line)),
                Err(_) => break,
            }
        }
    });
}

pub(crate) async fn wait_for_runtime_readiness<C: ManagedChild>(
    readiness: Option<&TokioProcessLauncher>,
    timeout: Duration,
    command: &CommandSpec,
    child: &mut C,
) -> Result<(), ProcessError> {
    let Some(readiness) = readiness else {
        return if child.has_exited()? {
            Err(ProcessError::ReadinessFailed(command.engine))
        } else {
            Ok(())
        };
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if child.has_exited()? {
            return Err(ProcessError::ReadinessFailed(command.engine));
        }
        if readiness.current_ready(command.engine).await {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ProcessError::ReadinessTimedOut(command.engine));
        }
        tokio::time::sleep(READINESS_POLL_INTERVAL).await;
    }
}

pub(crate) struct BoundedLogDecoder {
    line: Vec<u8>,
    overflowed: bool,
}

impl Default for BoundedLogDecoder {
    fn default() -> Self {
        Self {
            line: Vec::with_capacity(MAX_CAPTURED_LOG_LINE_BYTES),
            overflowed: false,
        }
    }
}

impl BoundedLogDecoder {
    pub(crate) fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(&str)) {
        for &byte in bytes {
            if byte == b'\n' {
                self.emit(&mut emit);
            } else if self.line.len() < MAX_CAPTURED_LOG_LINE_BYTES {
                if byte != b'\r' {
                    self.line.push(byte);
                }
            } else {
                self.overflowed = true;
            }
        }
    }

    pub(crate) fn finish(&mut self, mut emit: impl FnMut(&str)) {
        if !self.line.is_empty() || self.overflowed {
            self.emit(&mut emit);
        }
    }

    fn emit(&mut self, emit: &mut impl FnMut(&str)) {
        let mut text = String::from_utf8_lossy(&self.line).into_owned();
        if self.overflowed {
            text.push_str(" [truncated]");
        }
        emit(&text);
        self.line.clear();
        self.overflowed = false;
    }
}

fn xray_loopback_inbounds(path: &std::path::Path) -> Result<Vec<std::net::SocketAddr>, ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    let document: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let inbounds = document
        .get("inbounds")
        .and_then(serde_json::Value::as_array)
        .ok_or(())?;
    let mut addresses = Vec::new();
    for inbound in inbounds {
        let Some(port) = inbound.get("port").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let Ok(port) = u16::try_from(port) else {
            continue;
        };
        let listen = inbound
            .get("listen")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("127.0.0.1");
        let Ok(ip) = listen.parse::<std::net::IpAddr>() else {
            continue;
        };
        if ip.is_loopback() {
            addresses.push(std::net::SocketAddr::new(ip, port));
        }
    }
    Ok(addresses)
}

/// The runtime copy of `IDLE_XRAY_CONFIG` is re-serialized and may gain an
/// outbound interface, so it is recognised by shape: no inbound and only the
/// idle outbound.
fn xray_config_is_idle(path: &std::path::Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let no_inbounds = document
        .get("inbounds")
        .and_then(serde_json::Value::as_array)
        .is_none_or(Vec::is_empty);
    let idle_outbound = document
        .get("outbounds")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|outbounds| {
            outbounds.len() == 1
                && outbounds[0].get("tag").and_then(serde_json::Value::as_str)
                    == Some(crate::IDLE_XRAY_OUTBOUND_TAG)
        });
    no_inbounds && idle_outbound
}

/// Finds one loopback SOCKS inbound whose 1:1-tagged outbound requires DNS
/// resolution (its `address` is a domain, not a literal IP) - i.e. one of
/// this app's own endpoint-DNS-scoped outbounds, never anything
/// provider/user supplied outside that overlay. Returns the inbound to dial
/// through and the outbound's own domain:port to dial to. `None` means
/// nothing in this config needs a DNS warm-up (every outbound already
/// dials a literal IP), not an error.
fn xray_domain_warmup_target(
    path: &std::path::Path,
) -> Option<(std::net::SocketAddr, String, u16)> {
    let bytes = std::fs::read(path).ok()?;
    let document: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let inbounds = document.get("inbounds")?.as_array()?;
    let outbounds = document.get("outbounds")?.as_array()?;
    for inbound in inbounds {
        if inbound.get("protocol").and_then(serde_json::Value::as_str) != Some("socks") {
            continue;
        }
        let Some(tag) = inbound.get("tag").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(port) = inbound
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
        else {
            continue;
        };
        let listen = inbound
            .get("listen")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("127.0.0.1");
        let Ok(ip) = listen.parse::<std::net::IpAddr>() else {
            continue;
        };
        if !ip.is_loopback() {
            continue;
        }
        let Some(outbound) = outbounds
            .iter()
            .find(|outbound| outbound.get("tag").and_then(serde_json::Value::as_str) == Some(tag))
        else {
            continue;
        };
        // Most single-server protocols (shadowsocks, vmess, vless, trojan -
        // everything this app's own overlay generates) put `address`/`port`
        // directly under `settings`. A few (socks, http) nest the same pair
        // one level deeper under `settings.servers[0]`. Accept either shape
        // rather than assuming one, since this only has to find *a* domain
        // to warm up, not fully understand every outbound protocol.
        let settings = outbound.get("settings");
        let nested_server = settings
            .and_then(|settings| settings.get("servers"))
            .and_then(serde_json::Value::as_array)
            .and_then(|servers| servers.first());
        let target = settings.into_iter().chain(nested_server).find(|candidate| {
            candidate
                .get("address")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|address| address.parse::<std::net::IpAddr>().is_err())
        });
        let Some(target) = target else {
            continue;
        };
        let address = target
            .get("address")
            .and_then(serde_json::Value::as_str)
            .expect("matched by the address-is-a-domain predicate above");
        let Some(destination_port) = target
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
        else {
            continue;
        };
        return Some((
            std::net::SocketAddr::new(ip, port),
            address.to_owned(),
            destination_port,
        ));
    }
    None
}

/// Proves Xray's real DNS-resolve-then-dial path works, not just that its
/// SOCKS listener accepts a connection: a real SOCKS5 CONNECT through one
/// loopback inbound to its own outbound's real domain:port, so an actual
/// endpoint-DNS lookup happens. `None` from `xray_domain_warmup_target`
/// (nothing in this config needs DNS) is treated as already warm.
///
/// A live-hardware capture (real pinned Xray, real endpoint domain) proved
/// the client-facing SOCKS5 CONNECT reply is sent the instant Xray accepts
/// the request into its dispatcher, entirely independent of whether the
/// real DNS lookup or outbound dial ever succeeds: the "success" header
/// arrived in well under a second while a deliberately-blocked scoped
/// resolver still hadn't answered. Only once Xray's own configured DNS
/// timeoutMs genuinely elapsed did it close the client-facing connection to
/// report the resolve-then-dial had failed. So the reply header alone is
/// not proof of readiness. This instead treats that reply as a mere
/// acknowledgement and watches the connection for that failure-close signal
/// over a window bounded by Xray's own DNS timeout (`ENDPOINT_DNS_TIMEOUT_MS`,
/// shared with the config generator so the two can never drift apart): if
/// the connection is still alive once that window elapses, the resolve-
/// then-dial genuinely succeeded.
async fn xray_domain_dns_warm(config_path: &std::path::Path) -> bool {
    use tokio::io::AsyncWriteExt;

    let Some((inbound, domain, destination_port)) = xray_domain_warmup_target(config_path) else {
        return true;
    };
    let Ok(Ok(mut stream)) =
        tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(inbound)).await
    else {
        return false;
    };
    if stream.write_all(&[0x05, 0x01, 0x00]).await.is_err() {
        return false;
    }
    let mut greeting = [0_u8; 2];
    if tokio::time::timeout(Duration::from_millis(500), stream.read_exact(&mut greeting))
        .await
        .is_err()
    {
        return false;
    }
    if greeting != [0x05, 0x00] {
        return false;
    }
    let domain_bytes = domain.as_bytes();
    if domain_bytes.len() > u8::MAX as usize {
        return false;
    }
    let mut request = vec![0x05, 0x01, 0x00, 0x03, domain_bytes.len() as u8];
    request.extend_from_slice(domain_bytes);
    request.extend_from_slice(&destination_port.to_be_bytes());
    if stream.write_all(&request).await.is_err() {
        return false;
    }

    let mut reply_head = [0_u8; 4];
    if tokio::time::timeout(
        Duration::from_millis(2_000),
        stream.read_exact(&mut reply_head),
    )
    .await
    .is_err()
    {
        return false;
    }
    if reply_head[1] != 0x00 {
        return false;
    }
    // Drain BND.ADDR + BND.PORT per ATYP so nothing is left unread to be
    // mistaken for the later close/data signal being watched for below.
    let addr_len: usize = match reply_head[3] {
        0x01 => 4,
        0x03 => {
            let mut len = [0_u8; 1];
            if tokio::time::timeout(Duration::from_millis(500), stream.read_exact(&mut len))
                .await
                .is_err()
            {
                return false;
            }
            len[0] as usize
        }
        0x04 => 16,
        _ => return false,
    };
    let mut rest = vec![0_u8; addr_len + 2];
    if tokio::time::timeout(Duration::from_millis(500), stream.read_exact(&mut rest))
        .await
        .is_err()
    {
        return false;
    }

    let confirm_window = Duration::from_millis(crate::snapshot::ENDPOINT_DNS_TIMEOUT_MS)
        + Duration::from_millis(300);
    let deadline = tokio::time::Instant::now() + confirm_window;
    let mut probe_buf = [0_u8; 64];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return true;
        }
        match tokio::time::timeout(remaining, stream.read(&mut probe_buf)).await {
            Ok(Ok(0)) => return false,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => return false,
            Err(_) => return true,
        }
    }
}

/// Where application traffic enters Mihomo: its TUN device, or (system-proxy mode, TUN
/// disabled) its loopback mixed port. A service TUN generation starts with TUN disabled and no
/// inbound port at all (TUN is switched on later over REST), so only its controller counts.
#[derive(Debug, PartialEq, Eq)]
enum MihomoTrafficEntry {
    Tun { device: String },
    MixedPort(std::net::SocketAddr),
    ControllerOnly,
}

#[derive(Debug, PartialEq, Eq)]
struct MihomoReadinessTargets {
    controller: std::net::SocketAddr,
    traffic: MihomoTrafficEntry,
}

fn mihomo_readiness_targets(paths: &RuntimePaths) -> Result<MihomoReadinessTargets, ()> {
    let bytes = std::fs::read(&paths.mihomo_config).map_err(|_| ())?;
    mihomo_readiness_targets_from(&bytes, paths.mihomo_home_directory.is_some())
}

/// `service_mode`: the runtime is a service generation (Mihomo has its own home directory).
fn mihomo_readiness_targets_from(
    bytes: &[u8],
    service_mode: bool,
) -> Result<MihomoReadinessTargets, ()> {
    let document: serde_yaml_ng::Value = serde_yaml_ng::from_slice(bytes).map_err(|_| ())?;
    let mapping = document.as_mapping().ok_or(())?;
    let controller = mapping
        .get(serde_yaml_ng::Value::String(
            "external-controller".to_owned(),
        ))
        .and_then(serde_yaml_ng::Value::as_str)
        .ok_or(())?
        .parse::<std::net::SocketAddr>()
        .map_err(|_| ())?;
    if !controller.ip().is_loopback() {
        return Err(());
    }
    let key = |name: &str| serde_yaml_ng::Value::String(name.to_owned());
    let tun = mapping
        .get(key("tun"))
        .and_then(serde_yaml_ng::Value::as_mapping);
    let tun_disabled = tun
        .and_then(|tun| tun.get(key("enable")))
        .and_then(serde_yaml_ng::Value::as_bool)
        == Some(false);
    if tun_disabled {
        // Only the loopback-bound mixed port written by the proxy-mode overlay counts.
        if mapping
            .get(key("bind-address"))
            .and_then(serde_yaml_ng::Value::as_str)
            != Some("127.0.0.1")
        {
            return Err(());
        }
        if service_mode && mapping.get(key("mixed-port")).is_none() {
            return Ok(MihomoReadinessTargets {
                controller,
                traffic: MihomoTrafficEntry::ControllerOnly,
            });
        }
        let port = mapping
            .get(key("mixed-port"))
            .and_then(serde_yaml_ng::Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or(())?;
        return Ok(MihomoReadinessTargets {
            controller,
            traffic: MihomoTrafficEntry::MixedPort(std::net::SocketAddr::from((
                std::net::Ipv4Addr::LOCALHOST,
                port,
            ))),
        });
    }
    let device = tun
        .and_then(|tun| tun.get(key("device")))
        .and_then(serde_yaml_ng::Value::as_str)
        .filter(|device| !device.trim().is_empty())
        .ok_or(())?;
    Ok(MihomoReadinessTargets {
        controller,
        traffic: MihomoTrafficEntry::Tun {
            device: device.to_owned(),
        },
    })
}

#[cfg(windows)]
async fn tun_device_is_up(device: &str) -> bool {
    use std::{ffi::c_void, ptr, slice};
    use windows_sys::Win32::{
        Foundation::NO_ERROR,
        NetworkManagement::{
            IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2},
            Ndis::NET_IF_OPER_STATUS_UP,
        },
    };
    if device != "MultiCore" {
        return false;
    }
    struct Table(*const c_void);
    impl Drop for Table {
        fn drop(&mut self) {
            unsafe { FreeMibTable(self.0) }
        }
    }
    let mut table: *mut MIB_IF_TABLE2 = ptr::null_mut();
    if unsafe { GetIfTable2(&mut table) } != NO_ERROR || table.is_null() {
        return false;
    }
    let _table = Table(table.cast());
    let rows =
        unsafe { slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
    rows.iter().any(|row| {
        let end = row
            .Alias
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(row.Alias.len());
        String::from_utf16(&row.Alias[..end])
            .is_ok_and(|alias| adapter_is_ready(&alias, row.OperStatus == NET_IF_OPER_STATUS_UP))
    })
}

#[cfg(any(windows, test))]
fn adapter_is_ready(alias: &str, operational_up: bool) -> bool {
    alias == "MultiCore" && operational_up
}

#[cfg(not(windows))]
async fn tun_device_is_up(device: &str) -> bool {
    device == "MultiCore"
        && crate::network::read_linux_interface(device)
            .is_some_and(|interface| crate::network::interface_is_up(interface.flags))
}

/// Whether MultiCore's own TUN adapter (`MultiCore`) exists and is operationally up.
pub async fn multicore_tun_is_up() -> bool {
    tun_device_is_up("MultiCore").await
}

pub struct SidecarProcessController<L: ProcessLauncher = TokioProcessLauncher> {
    paths: RuntimePaths,
    launcher: L,
    children: Mutex<HashMap<Engine, L::Child>>,
}

impl SidecarProcessController<TokioProcessLauncher> {
    pub fn new(paths: RuntimePaths) -> Self {
        let launcher = TokioProcessLauncher::for_runtime(paths.clone(), CORE_READINESS_TIMEOUT);
        Self::with_launcher(paths, launcher)
    }

    pub fn new_with_logs(paths: RuntimePaths, logs: Arc<CoreLogBuffer>) -> Self {
        let launcher = TokioProcessLauncher::for_runtime_with_logs(
            paths.clone(),
            CORE_READINESS_TIMEOUT,
            logs,
        );
        Self::with_launcher(paths, launcher)
    }
}

impl<L: ProcessLauncher> SidecarProcessController<L> {
    pub fn with_launcher(paths: RuntimePaths, launcher: L) -> Self {
        Self {
            paths,
            launcher,
            children: Mutex::new(HashMap::new()),
        }
    }
}

impl<L: ProcessLauncher> ProcessController for SidecarProcessController<L> {
    type Error = ProcessError;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            if let Some(child) = children.get_mut(&engine) {
                if !child.has_exited()? {
                    return Ok(());
                }
                children.remove(&engine);
            }

            let command = self.paths.command(engine);
            let mut child = self.launcher.launch(command.clone()).await?;
            if child.has_exited()? {
                return Err(ProcessError::LaunchFailed(engine));
            }
            if let Err(error) = self.launcher.wait_until_ready(&command, &mut child).await {
                let _ = child.stop().await;
                return Err(error);
            }
            children.insert(engine, child);
            Ok(())
        })
    }

    fn start_failure_cause(&self, error: &Self::Error) -> crate::StartFailureCause {
        match error {
            ProcessError::ReadinessTimedOut(_) => crate::StartFailureCause::ReadinessTimedOut,
            _ => crate::StartFailureCause::CoreFailed,
        }
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let Some(mut child) = children.remove(&engine) else {
                return Ok(());
            };
            if let Err(error) = child.stop().await {
                children.insert(engine, child);
                return Err(error);
            }
            Ok(())
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async move {
            let mut children = self.children.lock().await;
            let mut xray_running = false;
            let mut mihomo_running = false;
            if let Some(child) = children.get_mut(&Engine::Xray) {
                xray_running = child.has_exited().is_ok_and(|exited| !exited);
            }
            if let Some(child) = children.get_mut(&Engine::Mihomo) {
                mihomo_running = child.has_exited().is_ok_and(|exited| !exited);
            }
            let xray = if xray_running {
                if self.launcher.current_ready(Engine::Xray).await {
                    RuntimeCheckState::Ready
                } else {
                    RuntimeCheckState::Failed
                }
            } else {
                RuntimeCheckState::Stopped
            };
            let mihomo = if mihomo_running {
                if self.launcher.current_ready(Engine::Mihomo).await {
                    RuntimeCheckState::Ready
                } else {
                    RuntimeCheckState::Failed
                }
            } else {
                RuntimeCheckState::Stopped
            };
            let tun = if mihomo_running {
                self.launcher.tun_check().await
            } else {
                RuntimeCheckState::Stopped
            };
            ProcessDiagnostics {
                xray,
                mihomo,
                tun,
                logs: self.launcher.diagnostic_logs(),
            }
        })
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;

    #[test]
    fn idle_xray_is_recognised_after_runtime_rewrites_but_provider_configs_are_not() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("xray.json");
        std::fs::write(&path, crate::IDLE_XRAY_CONFIG).unwrap();
        assert!(xray_config_is_idle(&path));
        std::fs::write(
            &path,
            br#"{"outbounds":[{"protocol":"freedom","streamSettings":{"sockopt":{"interface":"Ethernet"}},"tag":"multicore-idle"}]}"#,
        )
        .unwrap();
        assert!(xray_config_is_idle(&path));
        std::fs::write(
            &path,
            br#"{"inbounds":[],"outbounds":[{"protocol":"freedom","tag":"direct"}]}"#,
        )
        .unwrap();
        assert!(!xray_config_is_idle(&path));
    }

    #[test]
    fn readiness_targets_use_the_tun_device_by_default() {
        let targets = mihomo_readiness_targets_from(
            b"external-controller: 127.0.0.1:19090\ntun:\n  device: MultiCore\n",
            false,
        )
        .unwrap();
        assert_eq!(targets.controller, "127.0.0.1:19090".parse().unwrap());
        assert_eq!(
            targets.traffic,
            MihomoTrafficEntry::Tun {
                device: "MultiCore".into()
            }
        );
    }

    #[test]
    fn readiness_targets_without_tun_use_the_loopback_mixed_port() {
        let targets = mihomo_readiness_targets_from(
            b"external-controller: 127.0.0.1:19090\nmixed-port: 28765\nbind-address: 127.0.0.1\ntun:\n  enable: false\n  device: MultiCore\n",
            false,
        )
        .unwrap();
        assert_eq!(
            targets.traffic,
            MihomoTrafficEntry::MixedPort("127.0.0.1:28765".parse().unwrap())
        );
        // Disabled TUN without a loopback-bound proxy port is not a runnable runtime.
        for yaml in [
            &b"external-controller: 127.0.0.1:19090\ntun:\n  enable: false\n"[..],
            b"external-controller: 127.0.0.1:19090\nmixed-port: 7890\nbind-address: '*'\ntun:\n  enable: false\n",
            b"external-controller: 127.0.0.1:19090\nmixed-port: 0\nbind-address: 127.0.0.1\ntun:\n  enable: false\n",
        ] {
            assert!(mihomo_readiness_targets_from(yaml, false).is_err());
        }
    }

    #[test]
    fn service_readiness_without_any_port_is_the_controller_alone() {
        let no_port =
            b"external-controller: 127.0.0.1:19090\nbind-address: 127.0.0.1\ntun:\n  enable: false\n";
        assert_eq!(
            mihomo_readiness_targets_from(no_port, true)
                .unwrap()
                .traffic,
            MihomoTrafficEntry::ControllerOnly
        );
        assert!(mihomo_readiness_targets_from(no_port, false).is_err());
        let proxy_form = b"external-controller: 127.0.0.1:19090\nmixed-port: 28765\nbind-address: 127.0.0.1\ntun:\n  enable: false\n";
        assert_eq!(
            mihomo_readiness_targets_from(proxy_form, true)
                .unwrap()
                .traffic,
            MihomoTrafficEntry::MixedPort("127.0.0.1:28765".parse().unwrap())
        );
        for yaml in [
            &b"external-controller: 127.0.0.1:19090\nmixed-port: 0\nbind-address: 127.0.0.1\ntun:\n  enable: false\n"[..],
            b"external-controller: 127.0.0.1:19090\nbind-address: '*'\ntun:\n  enable: false\n",
        ] {
            assert!(mihomo_readiness_targets_from(yaml, true).is_err());
        }
    }

    #[test]
    fn adapter_requires_exact_alias_and_operational_up() {
        assert!(adapter_is_ready("MultiCore", true));
        assert!(!adapter_is_ready("MultiCore", false));
        assert!(!adapter_is_ready("multicore", true));
        assert!(!adapter_is_ready("MultiCore Evil", true));
    }

    #[test]
    fn unterminated_megabytes_are_bounded_discarded_and_marked() {
        let mut decoder = BoundedLogDecoder::default();
        let chunk = vec![b'A'; 8192];
        let mut records = Vec::new();
        for _ in 0..256 {
            decoder.feed(&chunk, |line| records.push(line.to_owned()));
        }
        decoder.finish(|line| records.push(line.to_owned()));
        assert_eq!(records.len(), 1);
        assert!(records[0].ends_with(" [truncated]"));
        assert!(records[0].len() <= MAX_CAPTURED_LOG_LINE_BYTES + 32);
    }

    #[test]
    fn huge_unterminated_child_output_is_redacted_before_diagnostics() {
        let logs = CoreLogBuffer::with_limits(2, 2048);
        let mut decoder = BoundedLogDecoder::default();
        decoder.feed(b"https://secret.example/token ", |line| {
            logs.push(Engine::Xray, DiagnosticStream::Stderr, line)
        });
        let chunk = vec![b'X'; 8192];
        for _ in 0..256 {
            decoder.feed(&chunk, |line| {
                logs.push(Engine::Xray, DiagnosticStream::Stderr, line)
            });
        }
        decoder.finish(|line| logs.push(Engine::Xray, DiagnosticStream::Stderr, line));
        let records = logs.snapshot();
        assert_eq!(records.len(), 1);
        assert!(!records[0].message.contains("secret.example"));
        assert!(records[0].message.chars().count() <= 2048);
    }
}
