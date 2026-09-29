//! Connection modes: TUN (all traffic, cores run in the elevated host) or system proxy (TUN
//! off, cores run as the user with no administrator prompt, and the OS proxy settings point at
//! Mihomo's loopback mixed port while connected).

use std::{future::Future, pin::Pin, sync::Arc};

use multicore_core::{Engine, ProcessController, ProcessDiagnostics, StartFailureCause};

/// A connection's controller in either mode.
pub enum ModeController<T, L> {
    Tun(T),
    Proxy(L),
}

#[derive(Debug)]
pub enum ModeError<T, L> {
    Tun(T),
    Proxy(L),
}

impl<T: ProcessController, L: ProcessController> ProcessController for ModeController<T, L> {
    type Error = ModeError<T::Error, L::Error>;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        match self {
            Self::Tun(controller) => {
                Box::pin(async move { controller.start(engine).await.map_err(ModeError::Tun) })
            }
            Self::Proxy(controller) => {
                Box::pin(async move { controller.start(engine).await.map_err(ModeError::Proxy) })
            }
        }
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        match self {
            Self::Tun(controller) => {
                Box::pin(async move { controller.stop(engine).await.map_err(ModeError::Tun) })
            }
            Self::Proxy(controller) => {
                Box::pin(async move { controller.stop(engine).await.map_err(ModeError::Proxy) })
            }
        }
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        match self {
            Self::Tun(controller) => controller.diagnostics(),
            Self::Proxy(controller) => controller.diagnostics(),
        }
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        match (self, error) {
            (Self::Tun(controller), ModeError::Tun(error)) => controller.start_failure_cause(error),
            (Self::Proxy(controller), ModeError::Proxy(error)) => {
                controller.start_failure_cause(error)
            }
            _ => StartFailureCause::Unknown,
        }
    }
}

/// Turns the OS proxy on and off; see `system_proxy::SystemProxy`.
pub trait ProxySwitch: Send + Sync {
    fn apply(&self) -> std::io::Result<()>;
    fn restore(&self) -> std::io::Result<()>;
}

#[derive(Debug)]
pub enum ProxyModeError<E> {
    Core(E),
    /// Mihomo started but the system proxy could not be pointed at it.
    SystemProxy,
}

/// The proxy-mode controller: the system proxy follows Mihomo's lifetime. It is applied only
/// once Mihomo is ready (so apps never meet a dead proxy) and restored before Mihomo stops.
pub struct SystemProxyController<P> {
    inner: P,
    proxy: Arc<dyn ProxySwitch>,
}

impl<P> SystemProxyController<P> {
    pub fn new(inner: P, proxy: Arc<dyn ProxySwitch>) -> Self {
        Self { inner, proxy }
    }
}

impl<P: ProcessController> ProcessController for SystemProxyController<P> {
    type Error = ProxyModeError<P::Error>;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.inner
                .start(engine)
                .await
                .map_err(ProxyModeError::Core)?;
            if engine == Engine::Mihomo && self.proxy.apply().is_err() {
                // The supervisor rolls back what it started; Mihomo is stopped here so the
                // failed start leaves nothing of its own running.
                let _ = self.inner.stop(engine).await;
                return Err(ProxyModeError::SystemProxy);
            }
            Ok(())
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Mihomo
                && let Err(error) = self.proxy.restore()
            {
                // Kept on disk; the next daemon start retries the restore.
                eprintln!("system proxy restore failed: {error}");
            }
            self.inner.stop(engine).await.map_err(ProxyModeError::Core)
        })
    }

    fn diagnostics<'a>(&'a self) -> Pin<Box<dyn Future<Output = ProcessDiagnostics> + Send + 'a>> {
        self.inner.diagnostics()
    }

    fn start_failure_cause(&self, error: &Self::Error) -> StartFailureCause {
        match error {
            ProxyModeError::Core(error) => self.inner.start_failure_cause(error),
            ProxyModeError::SystemProxy => StartFailureCause::CoreFailed,
        }
    }
}

/// Stands in for the proxy-mode controller on platforms without one (TUN is always used).
pub enum NoProxyController {}

impl ProcessController for NoProxyController {
    type Error = std::convert::Infallible;

    fn start<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        match *self {}
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        fail_apply: bool,
    }

    impl Recorder {
        fn log(&self, call: &str) {
            self.calls.lock().unwrap().push(call.to_owned());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ProxySwitch for Recorder {
        fn apply(&self) -> std::io::Result<()> {
            self.log("apply");
            if self.fail_apply {
                return Err(std::io::Error::other("denied"));
            }
            Ok(())
        }
        fn restore(&self) -> std::io::Result<()> {
            self.log("restore");
            Ok(())
        }
    }

    struct Cores(Arc<Recorder>);

    impl ProcessController for Cores {
        type Error = ();

        fn start<'a>(
            &'a self,
            engine: Engine,
        ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
            self.0.log(&format!("start {engine:?}"));
            Box::pin(async { Ok(()) })
        }

        fn stop<'a>(
            &'a self,
            engine: Engine,
        ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
            self.0.log(&format!("stop {engine:?}"));
            Box::pin(async { Ok(()) })
        }
    }

    fn controller(recorder: &Arc<Recorder>) -> SystemProxyController<Cores> {
        SystemProxyController::new(Cores(recorder.clone()), recorder.clone())
    }

    #[tokio::test]
    async fn the_proxy_is_applied_after_mihomo_is_ready_and_restored_before_it_stops() {
        let recorder = Arc::new(Recorder::default());
        let controller = controller(&recorder);

        controller.start(Engine::Xray).await.unwrap();
        controller.start(Engine::Mihomo).await.unwrap();
        controller.stop(Engine::Mihomo).await.unwrap();
        controller.stop(Engine::Xray).await.unwrap();

        assert_eq!(
            recorder.calls(),
            [
                "start Xray",
                "start Mihomo",
                "apply",
                "restore",
                "stop Mihomo",
                "stop Xray"
            ]
        );
    }

    #[tokio::test]
    async fn a_proxy_that_cannot_be_applied_fails_the_start_and_stops_mihomo() {
        let recorder = Arc::new(Recorder {
            fail_apply: true,
            ..Recorder::default()
        });
        let controller = controller(&recorder);

        let error = controller.start(Engine::Mihomo).await.unwrap_err();
        assert!(matches!(error, ProxyModeError::SystemProxy));
        assert_eq!(
            controller.start_failure_cause(&error),
            StartFailureCause::CoreFailed
        );
        assert_eq!(recorder.calls(), ["start Mihomo", "apply", "stop Mihomo"]);
    }

    #[tokio::test]
    async fn mode_controller_delegates_to_the_selected_mode() {
        let recorder = Arc::new(Recorder::default());
        let mode: ModeController<NoProxyController, _> =
            ModeController::Proxy(controller(&recorder));
        mode.start(Engine::Mihomo).await.unwrap();
        assert_eq!(recorder.calls(), ["start Mihomo", "apply"]);
    }
}
