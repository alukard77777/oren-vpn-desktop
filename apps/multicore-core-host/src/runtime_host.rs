use std::{future::Future, pin::Pin, sync::Arc};

use multicore_core::{
    CoreLogBuffer, Engine, ProcessController, ProcessDiagnostics, ProcessError, RuntimeCheckState,
    SidecarProcessController,
    elevation_protocol::{
        BrokerErrorCode, ElevatedDiagnosticStream, ElevatedEngine, ElevatedLogRecord,
        ElevatedRuntimeState, ElevationCommand, ElevationResponse, MAX_ELEVATION_FRAME_BYTES,
        encode_frame,
    },
};

#[cfg(windows)]
use crate::runtime_paths::{ResolvedRuntime, resolve_current};
#[cfg(unix)]
use crate::unix_runtime_paths::{ResolvedRuntime, resolve_current};
#[cfg(unix)]
use multicore_core::UnixGroupLauncher as PlatformLauncher;
#[cfg(windows)]
use multicore_core::WindowsJobLauncher as PlatformLauncher;

trait Controller: Send + Sync {
    /// Starts `engine`, or confirms it is still running. An exited core is started again.
    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), BrokerErrorCode>> + Send + 'a>>;
    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>>;
    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>>;
}

struct ProductionController {
    controller: SidecarProcessController<PlatformLauncher>,
    _validated_files: ResolvedRuntime,
}
impl Controller for ProductionController {
    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), BrokerErrorCode>> + Send + 'a>> {
        Box::pin(async move {
            self._validated_files
                .verify_unchanged()
                .map_err(|_| BrokerErrorCode::NotReady)?;
            #[cfg(windows)]
            if engine == Engine::Mihomo
                && let crate::wintun_cleanup::PhantomCleanup::Failed(code) =
                    crate::wintun_cleanup::remove_phantom_tun_adapter()
            {
                // Best effort: Mihomo still starts, only slower (see the module docs).
                eprintln!("stale TUN adapter could not be removed (CONFIGRET {code})");
            }
            self.controller
                .start(engine)
                .await
                .map_err(|error| match error {
                    ProcessError::ReadinessTimedOut(_) => BrokerErrorCode::TimedOut,
                    _ => BrokerErrorCode::NotReady,
                })
        })
    }
    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move { self.controller.stop(engine).await.map_err(|_| ()) })
    }
    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        self.controller.diagnostics()
    }
}

trait Factory: Send + Sync {
    fn create(&self, generation: u64) -> Result<Box<dyn Controller>, ()>;
}
struct ProductionFactory;
impl Factory for ProductionFactory {
    fn create(&self, generation: u64) -> Result<Box<dyn Controller>, ()> {
        let resolved = resolve_current(generation).map_err(|_| ())?;
        // Best-effort: a file the local debugging/support story can actually
        // read without the desktop's per-session API token in hand. Every
        // line is already redacted before it ever reaches the buffer (see
        // CoreLogBuffer::push), so persisting it carries no new secret-
        // exposure risk; if the file can't be created for any reason this
        // silently falls back to memory-only, matching this being
        // diagnostic-only and never allowed to block a real Connect.
        let log_path = resolved.data_root.join("logs").join("latest-core.log");
        let logs = Arc::new(CoreLogBuffer::persistent_or_memory(log_path));
        let launcher = PlatformLauncher::for_runtime(
            resolved.paths.clone(),
            logs,
            multicore_core::CORE_READINESS_TIMEOUT,
        )
        .map_err(|_| ())?;
        let controller = SidecarProcessController::with_launcher(resolved.paths.clone(), launcher);
        Ok(Box::new(ProductionController {
            controller,
            _validated_files: resolved,
        }))
    }
}

pub(crate) struct RuntimeHost {
    runtime: tokio::runtime::Runtime,
    factory: Box<dyn Factory>,
    active: Option<Active>,
}
struct Active {
    generation: u64,
    controller: Box<dyn Controller>,
    xray: bool,
    mihomo: bool,
}

impl RuntimeHost {
    pub(crate) fn production() -> Result<Self, ()> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| ())?,
            factory: Box::new(ProductionFactory),
            active: None,
        })
    }
    pub(crate) fn execute(&mut self, command: ElevationCommand) -> ElevationResponse {
        match command {
            ElevationCommand::StartXray { generation_id } => {
                self.start(generation_id, Engine::Xray)
            }
            ElevationCommand::StartMihomo { generation_id } => {
                self.start(generation_id, Engine::Mihomo)
            }
            ElevationCommand::Stop { engine } => {
                if self.stop(to_engine(engine)) {
                    ElevationResponse::Stopped { engine }
                } else {
                    ElevationResponse::Error {
                        code: BrokerErrorCode::NotReady,
                    }
                }
            }
            ElevationCommand::Diagnostics => self.diagnostics(),
            ElevationCommand::Shutdown => {
                self.shutdown();
                ElevationResponse::ShuttingDown
            }
        }
    }
    fn start(&mut self, generation: u64, engine: Engine) -> ElevationResponse {
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.generation != generation)
        {
            if self.any_core_alive() {
                return ElevationResponse::Error {
                    code: BrokerErrorCode::GenerationMismatch,
                };
            }
            // Every core of the previous generation has exited; release it for the new one.
            self.discard_active();
        }
        if engine == Engine::Mihomo && self.active.is_none() {
            return ElevationResponse::Error {
                code: BrokerErrorCode::NotReady,
            };
        }
        if self.active.is_none() {
            let Ok(controller) = self.factory.create(generation) else {
                return ElevationResponse::Error {
                    code: BrokerErrorCode::NotReady,
                };
            };
            self.active = Some(Active {
                generation,
                controller,
                xray: false,
                mihomo: false,
            });
        }
        let active = self.active.as_mut().unwrap();
        if engine == Engine::Mihomo && !active.xray {
            return ElevationResponse::Error {
                code: BrokerErrorCode::NotReady,
            };
        }
        // Always delegate: the controller is idempotent for a live core and restarts one that
        // exited, so a crashed core never stays reported as running.
        let result = self.runtime.block_on(active.controller.start(engine));
        let running = result.is_ok();
        match engine {
            Engine::Xray => active.xray = running,
            Engine::Mihomo => active.mihomo = running,
        }
        match result {
            Ok(()) => ElevationResponse::Started {
                engine: from_engine(engine),
            },
            Err(code) => {
                if !active.xray && !active.mihomo {
                    self.active = None;
                }
                ElevationResponse::Error { code }
            }
        }
    }
    fn any_core_alive(&self) -> bool {
        let Some(active) = self.active.as_ref() else {
            return false;
        };
        if !active.xray && !active.mihomo {
            return false;
        }
        let diagnostics = self.runtime.block_on(active.controller.diagnostics());
        diagnostics.xray != RuntimeCheckState::Stopped
            || diagnostics.mihomo != RuntimeCheckState::Stopped
    }
    fn discard_active(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = self
                .runtime
                .block_on(active.controller.stop(Engine::Mihomo));
            let _ = self.runtime.block_on(active.controller.stop(Engine::Xray));
        }
    }
    fn stop(&mut self, engine: Engine) -> bool {
        if engine == Engine::Xray
            && self.active.as_ref().is_some_and(|active| active.mihomo)
            && !self.stop(Engine::Mihomo)
        {
            return false;
        }
        let Some(active) = self.active.as_mut() else {
            return true;
        };
        let running = match engine {
            Engine::Xray => active.xray,
            Engine::Mihomo => active.mihomo,
        };
        if running
            && self
                .runtime
                .block_on(active.controller.stop(engine))
                .is_err()
        {
            return false;
        }
        if running {
            match engine {
                Engine::Xray => active.xray = false,
                Engine::Mihomo => active.mihomo = false,
            }
        }
        if !active.xray && !active.mihomo {
            self.active = None;
        }
        true
    }
    pub(crate) fn shutdown(&mut self) {
        let _ = self.stop(Engine::Mihomo);
        let _ = self.stop(Engine::Xray);
        self.active = None;
    }
    fn diagnostics(&mut self) -> ElevationResponse {
        let diagnostics = self
            .active
            .as_ref()
            .map_or_else(ProcessDiagnostics::default, |active| {
                self.runtime.block_on(active.controller.diagnostics())
            });
        let mut logs = diagnostics
            .logs
            .into_iter()
            .map(|record| ElevatedLogRecord {
                id: record.id,
                timestamp_ms: record.timestamp_ms,
                engine: from_engine(record.engine),
                stream: match record.stream {
                    multicore_core::DiagnosticStream::Stdout => ElevatedDiagnosticStream::Stdout,
                    multicore_core::DiagnosticStream::Stderr => ElevatedDiagnosticStream::Stderr,
                },
                message: broker_safe_log(&record.message),
            })
            .collect::<Vec<_>>();
        loop {
            let response = ElevationResponse::ProcessDiagnostics {
                xray: state(diagnostics.xray),
                mihomo: state(diagnostics.mihomo),
                tun: state(diagnostics.tun),
                logs: logs.clone(),
            };
            if encode_frame(&response).is_ok_and(|f| f.len() <= MAX_ELEVATION_FRAME_BYTES) {
                return response;
            }
            if logs.is_empty() {
                return ElevationResponse::Error {
                    code: BrokerErrorCode::NotReady,
                };
            }
            logs.remove(0);
        }
    }
}
impl Drop for RuntimeHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn to_engine(v: ElevatedEngine) -> Engine {
    match v {
        ElevatedEngine::Xray => Engine::Xray,
        ElevatedEngine::Mihomo => Engine::Mihomo,
    }
}
fn from_engine(v: Engine) -> ElevatedEngine {
    match v {
        Engine::Xray => ElevatedEngine::Xray,
        Engine::Mihomo => ElevatedEngine::Mihomo,
    }
}
fn state(v: RuntimeCheckState) -> ElevatedRuntimeState {
    match v {
        RuntimeCheckState::Stopped => ElevatedRuntimeState::Stopped,
        RuntimeCheckState::Starting => ElevatedRuntimeState::Starting,
        RuntimeCheckState::Ready => ElevatedRuntimeState::Ready,
        RuntimeCheckState::Failed => ElevatedRuntimeState::Failed,
        RuntimeCheckState::Unsupported => ElevatedRuntimeState::Unsupported,
    }
}

fn broker_safe_log(message: &str) -> String {
    let trimmed = message.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    if message.contains(":\\")
        || message.contains(":/")
        || message.contains("\\\\")
        || trimmed.starts_with(['{', '['])
        || lower.contains("\"outbounds\"")
        || lower.contains("proxies:")
    {
        "[redacted privileged diagnostic]".to_owned()
    } else {
        message.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(bool, Engine)>>,
        diagnostics: Mutex<ProcessDiagnostics>,
        running: Mutex<Vec<Engine>>,
        fail_start: Mutex<Option<(Engine, BrokerErrorCode)>>,
    }
    impl Fake {
        fn crash(&self, engine: Engine) {
            self.running.lock().unwrap().retain(|e| *e != engine);
        }
        fn state(&self, engine: Engine) -> RuntimeCheckState {
            if self.running.lock().unwrap().contains(&engine) {
                RuntimeCheckState::Ready
            } else {
                RuntimeCheckState::Stopped
            }
        }
    }
    impl Controller for Arc<Fake> {
        fn start<'a>(
            &'a self,
            e: Engine,
        ) -> Pin<Box<dyn Future<Output = Result<(), BrokerErrorCode>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((true, e));
                if let Some((engine, code)) = *self.fail_start.lock().unwrap()
                    && engine == e
                {
                    return Err(code);
                }
                let mut running = self.running.lock().unwrap();
                if !running.contains(&e) {
                    running.push(e);
                }
                Ok(())
            })
        }
        fn stop<'a>(
            &'a self,
            e: Engine,
        ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((false, e));
                self.crash(e);
                Ok(())
            })
        }
        fn diagnostics<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
            Box::pin(async {
                let mut diagnostics = self.diagnostics.lock().unwrap().clone();
                diagnostics.xray = self.state(Engine::Xray);
                diagnostics.mihomo = self.state(Engine::Mihomo);
                diagnostics
            })
        }
    }
    struct FakeFactory {
        fake: Arc<Fake>,
        creates: Arc<AtomicUsize>,
    }
    impl Factory for FakeFactory {
        fn create(&self, _: u64) -> Result<Box<dyn Controller>, ()> {
            self.creates.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(self.fake.clone()))
        }
    }
    fn host() -> (RuntimeHost, Arc<Fake>, Arc<AtomicUsize>) {
        let fake = Arc::new(Fake::default());
        let creates = Arc::new(AtomicUsize::new(0));
        (
            RuntimeHost {
                runtime: tokio::runtime::Runtime::new().unwrap(),
                factory: Box::new(FakeFactory {
                    fake: fake.clone(),
                    creates: creates.clone(),
                }),
                active: None,
            },
            fake,
            creates,
        )
    }
    #[test]
    fn enforces_order_generation_and_idempotence() {
        let (mut h, f, c) = host();
        assert!(matches!(
            h.execute(ElevationCommand::StartMihomo { generation_id: 1 }),
            ElevationResponse::Error { .. }
        ));
        assert!(matches!(
            h.execute(ElevationCommand::StartXray { generation_id: 1 }),
            ElevationResponse::Started { .. }
        ));
        assert!(matches!(
            h.execute(ElevationCommand::StartXray { generation_id: 1 }),
            ElevationResponse::Started { .. }
        ));
        assert!(matches!(
            h.execute(ElevationCommand::StartMihomo { generation_id: 2 }),
            ElevationResponse::Error {
                code: BrokerErrorCode::GenerationMismatch
            }
        ));
        assert_eq!(c.load(Ordering::Relaxed), 1);
        // A repeated start is delegated; the controller itself is idempotent for a live core.
        assert_eq!(
            f.calls.lock().unwrap().iter().filter(|(s, _)| *s).count(),
            2
        );
        assert_eq!(f.running.lock().unwrap().as_slice(), [Engine::Xray]);
    }
    #[test]
    fn crashed_core_is_restarted_for_the_same_generation() {
        let (mut h, f, c) = host();
        h.execute(ElevationCommand::StartXray { generation_id: 1 });
        h.execute(ElevationCommand::StartMihomo { generation_id: 1 });
        f.crash(Engine::Mihomo);
        assert!(matches!(
            h.execute(ElevationCommand::StartMihomo { generation_id: 1 }),
            ElevationResponse::Started {
                engine: ElevatedEngine::Mihomo
            }
        ));
        assert_eq!(f.state(Engine::Mihomo), RuntimeCheckState::Ready);
        assert_eq!(c.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn exited_generation_does_not_block_a_new_generation() {
        let (mut h, f, c) = host();
        h.execute(ElevationCommand::StartXray { generation_id: 1 });
        h.execute(ElevationCommand::StartMihomo { generation_id: 1 });
        f.crash(Engine::Mihomo);
        f.crash(Engine::Xray);
        assert!(matches!(
            h.execute(ElevationCommand::StartXray { generation_id: 2 }),
            ElevationResponse::Started {
                engine: ElevatedEngine::Xray
            }
        ));
        assert_eq!(c.load(Ordering::Relaxed), 2);
        assert_eq!(h.active.as_ref().map(|a| a.generation), Some(2));
    }
    #[test]
    fn failed_restart_clears_the_running_flag_and_reports_the_code() {
        let (mut h, f, _) = host();
        h.execute(ElevationCommand::StartXray { generation_id: 1 });
        f.crash(Engine::Xray);
        *f.fail_start.lock().unwrap() = Some((Engine::Xray, BrokerErrorCode::TimedOut));
        assert!(matches!(
            h.execute(ElevationCommand::StartXray { generation_id: 1 }),
            ElevationResponse::Error {
                code: BrokerErrorCode::TimedOut
            }
        ));
        assert!(h.active.is_none());
    }
    #[test]
    fn stop_and_shutdown_are_idempotent() {
        let (mut h, f, _) = host();
        h.execute(ElevationCommand::StartXray { generation_id: 1 });
        h.execute(ElevationCommand::Stop {
            engine: ElevatedEngine::Xray,
        });
        h.execute(ElevationCommand::Stop {
            engine: ElevatedEngine::Xray,
        });
        h.shutdown();
        assert_eq!(
            f.calls.lock().unwrap().iter().filter(|(s, _)| !*s).count(),
            1
        );
    }
    #[test]
    fn broker_diagnostics_suppress_paths_and_config_fragments() {
        assert_eq!(
            broker_safe_log(r"failed C:\Users\name\xray.json"),
            "[redacted privileged diagnostic]"
        );
        assert_eq!(
            broker_safe_log(r#"{"outbounds":[]}"#),
            "[redacted privileged diagnostic]"
        );
        assert_eq!(broker_safe_log("core ready"), "core ready");
    }
    #[test]
    fn diagnostics_are_trimmed_to_one_broker_frame() {
        let (mut host, fake, _) = host();
        host.execute(ElevationCommand::StartXray { generation_id: 1 });
        fake.diagnostics.lock().unwrap().logs = (0..200)
            .map(|id| multicore_core::CoreLogRecord {
                id,
                timestamp_ms: id,
                engine: Engine::Xray,
                stream: multicore_core::DiagnosticStream::Stderr,
                message: "x".repeat(1024),
            })
            .collect();
        let response = host.execute(ElevationCommand::Diagnostics);
        assert!(encode_frame(&response).unwrap().len() <= MAX_ELEVATION_FRAME_BYTES);
        assert!(matches!(
            response,
            ElevationResponse::ProcessDiagnostics { .. }
        ));
    }
}
