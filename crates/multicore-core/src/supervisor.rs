use std::{future::Future, pin::Pin, sync::Arc};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Xray,
    Mihomo,
}

pub trait ProcessController: Send + Sync {
    type Error: Send;

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    fn diagnostics<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = crate::sidecar::ProcessDiagnostics> + Send + 'a>> {
        Box::pin(async { crate::sidecar::ProcessDiagnostics::default() })
    }

    /// Classifies a failed `start` so callers can explain it without seeing controller internals.
    fn start_failure_cause(&self, _error: &Self::Error) -> StartFailureCause {
        StartFailureCause::Unknown
    }
}

/// Why a sidecar could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartFailureCause {
    /// The controller did not classify the failure.
    Unknown,
    /// The user declined the administrator prompt (UAC or polkit).
    ElevationCancelled,
    /// The privileged host could not be launched or authorized.
    ElevationFailed,
    /// The privileged host session failed or stopped responding.
    BrokerFailed,
    /// The core process could not be launched or exited during startup.
    CoreFailed,
    /// The core stayed alive but did not become ready in time.
    ReadinessTimedOut,
    /// The always-ready service is held by another interactive user's session.
    ServiceBusy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SupervisorError {
    #[error("failed to start {engine:?}")]
    StartFailed {
        engine: Engine,
        rollback_failed: bool,
        cause: StartFailureCause,
    },
    #[error("failed to stop one or more sidecars")]
    StopFailed,
}

pub struct Supervisor<P: ProcessController + ?Sized> {
    controller: Arc<P>,
    operation: tokio::sync::Mutex<()>,
}

impl<P: ProcessController + ?Sized> Supervisor<P> {
    pub fn new(controller: Arc<P>) -> Self {
        Self {
            controller,
            operation: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn connect(&self) -> Result<(), SupervisorError> {
        let _operation = self.operation.lock().await;
        if let Err(error) = self.controller.start(Engine::Xray).await {
            return Err(SupervisorError::StartFailed {
                engine: Engine::Xray,
                rollback_failed: false,
                cause: self.controller.start_failure_cause(&error),
            });
        }

        if let Err(error) = self.controller.start(Engine::Mihomo).await {
            let cause = self.controller.start_failure_cause(&error);
            let rollback_failed = self.controller.stop(Engine::Xray).await.is_err();
            return Err(SupervisorError::StartFailed {
                engine: Engine::Mihomo,
                rollback_failed,
                cause,
            });
        }
        Ok(())
    }

    pub async fn disconnect(&self) -> Result<(), SupervisorError> {
        let _operation = self.operation.lock().await;
        let mihomo = self.controller.stop(Engine::Mihomo).await;
        let xray = self.controller.stop(Engine::Xray).await;
        if mihomo.is_err() || xray.is_err() {
            Err(SupervisorError::StopFailed)
        } else {
            Ok(())
        }
    }
}
