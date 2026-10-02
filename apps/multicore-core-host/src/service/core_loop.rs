//! Platform-neutral lifecycle of the always-ready service. The platform server resolves who is
//! on each connection; this loop decides what that client may do and drives the cores.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use multicore_core::elevation_protocol::{BrokerErrorCode, ElevatedRuntimeState};
use multicore_core::service_protocol::{ControllerEndpoint, ServiceCommand, ServiceResponse};
use multicore_core::troubleshoot::{FixOutcome, PrivilegedFix};

/// Who is on the other end of one connection, as established by the platform server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClientIdentity {
    /// Same user in the same session: Windows `"<session>:<sid>"`, Unix `"uid:<uid>"`.
    pub(crate) user_key: String,
    /// The user's MultiCore data directory, resolved from the client's token / uid.
    pub(crate) data_root: PathBuf,
    /// Unix: the uid whose ownership the runtime resolver enforces. Windows: 0.
    pub(crate) uid: u32,
    /// Windows: the client's user is an administrator (a UAC-filtered token counts). Only
    /// such clients may run troubleshooting fixes through the service; others are sent to
    /// the UAC prompt, which asks for an administrator's consent. Unix: false.
    pub(crate) administrator: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionId(pub(crate) u64);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CoreHealth {
    pub(crate) xray: bool,
    pub(crate) mihomo: bool,
}

impl CoreHealth {
    fn all(self) -> bool {
        self.xray && self.mihomo
    }
}

pub(crate) trait Cores: Send {
    /// Copies, sanitises and starts `generation` of `client` (stopping whatever ran before),
    /// with TUN off. Returns the controller the daemon should use: the service's filtered
    /// controller proxy, never Mihomo's own controller secret.
    fn prepare(
        &mut self,
        client: &ClientIdentity,
        generation: u64,
    ) -> Result<ControllerEndpoint, BrokerErrorCode>;
    /// Restarts both cores of the current generation with TUN off.
    fn restart(&mut self) -> Result<(), BrokerErrorCode>;
    fn set_tun(&mut self, on: bool) -> Result<(), BrokerErrorCode>;
    /// Whether TUN is actually on right now, when the cores can tell (`None`: unknown).
    fn tun_is_on(&mut self) -> Option<bool> {
        None
    }
    fn health(&mut self) -> CoreHealth;
    fn stop_all(&mut self);
    fn diagnostics(&mut self) -> ServiceResponse;
}

/// Service-side update: download and verify, then hand over to the updater and stop the service.
pub(crate) trait ServiceUpdater: Send {
    /// Downloads and verifies `version` while the cores keep running; a failure here leaves
    /// the tunnel untouched. Runs for both `StageUpdate` and `Update`, so a release an earlier
    /// `StageUpdate` already verified should be reused rather than downloaded again.
    fn prepare(&mut self, _client: &ClientIdentity, _version: &str) -> Result<(), BrokerErrorCode> {
        Ok(())
    }
    /// Starts the updater for the prepared `version` (the cores are already stopped) and
    /// arranges for the service to exit so the updater can replace `current`.
    fn begin(&mut self, client: &ClientIdentity, version: &str) -> Result<(), BrokerErrorCode>;
}

/// Service-side «Устранить неполадки»: runs a validated batch of fixed fixes and reports one
/// outcome per fix in request order.
pub(crate) trait Troubleshooter: Send + Sync {
    fn run(&self, fixes: &[PrivilegedFix]) -> Vec<FixOutcome>;
}

struct Owner {
    user_key: String,
    connection: ConnectionId,
}

/// Restarts allowed per prepared generation before a crash is reported.
const MAX_CRASH_RESTARTS: u8 = 1;

pub(crate) struct ServiceLoop<C: Cores> {
    cores: C,
    owner: Option<Owner>,
    prepared: Option<(u64, ControllerEndpoint)>,
    tun_on: bool,
    crash_restarts: u8,
    failure: Option<BrokerErrorCode>,
    updater: Option<Box<dyn ServiceUpdater>>,
    /// An `Update` is downloading outside the lock; its updater is taken out meanwhile.
    updating: bool,
    troubleshooter: Option<Arc<dyn Troubleshooter>>,
    /// A `Troubleshoot` batch is running outside the lock.
    troubleshooting: bool,
    /// The service directory whose `logs/service.log*` `ServiceLog` serves.
    log_directory: Option<PathBuf>,
}

/// An `Update` that passed the ownership check and holds the updater while it downloads
/// without the service-loop lock.
pub(crate) struct PendingUpdate {
    updater: Box<dyn ServiceUpdater>,
}

impl<C: Cores> ServiceLoop<C> {
    pub(crate) fn new(cores: C) -> Self {
        Self {
            cores,
            owner: None,
            prepared: None,
            tun_on: false,
            crash_restarts: 0,
            failure: None,
            updater: None,
            updating: false,
            troubleshooter: None,
            troubleshooting: false,
            log_directory: None,
        }
    }

    /// Serves `ServiceLog` from `<service_directory>/logs`.
    pub(crate) fn with_log_directory(mut self, service_directory: PathBuf) -> Self {
        self.log_directory = Some(service_directory);
        self
    }

    /// Windows only: Linux has no service-side troubleshooting.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn with_troubleshooter(mut self, troubleshooter: Arc<dyn Troubleshooter>) -> Self {
        self.troubleshooter = Some(troubleshooter);
        self
    }

    /// Windows only: Linux packages are updated by the package manager.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn with_updater(mut self, updater: Box<dyn ServiceUpdater>) -> Self {
        self.updater = Some(updater);
        self
    }

    #[allow(dead_code, reason = "inspected by the service tests")]
    pub(crate) fn cores_mut(&mut self) -> &mut C {
        &mut self.cores
    }

    #[allow(dead_code, reason = "inspected by the service tests")]
    pub(crate) fn owner_key(&self) -> Option<&str> {
        self.owner.as_ref().map(|owner| owner.user_key.as_str())
    }

    pub(crate) fn execute(
        &mut self,
        connection: ConnectionId,
        client: &ClientIdentity,
        command: ServiceCommand,
    ) -> ServiceResponse {
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.user_key != client.user_key)
        {
            return ServiceResponse::Busy;
        }
        match command {
            ServiceCommand::Prepare { generation_id } => {
                self.claim(connection, client);
                self.prepare(client, generation_id)
            }
            ServiceCommand::SetTun { on } => {
                self.claim(connection, client);
                self.set_tun(on)
            }
            ServiceCommand::Status => self.status(),
            ServiceCommand::Stop { engine } => {
                self.claim(connection, client);
                self.stop_cores();
                ServiceResponse::Stopped { engine }
            }
            ServiceCommand::Diagnostics => self.cores.diagnostics(),
            // Read-only and redacted; another user's session was turned away above, like for
            // every other command.
            ServiceCommand::ServiceLog { file, before } => match &self.log_directory {
                Some(directory) => super::service_log::read_chunk(directory, file, before),
                None => ServiceResponse::Error {
                    code: BrokerErrorCode::NotReady,
                },
            },
            ServiceCommand::Shutdown => {
                self.stop_everything();
                ServiceResponse::ShuttingDown
            }
            ServiceCommand::Update { version } => self.update(client, &version, false),
            ServiceCommand::StageUpdate { version } => self.update(client, &version, true),
            ServiceCommand::Troubleshoot { fixes } => match self.start_troubleshoot(client, &fixes)
            {
                Ok(troubleshooter) => {
                    let outcomes = troubleshooter.run(&fixes);
                    self.finish_troubleshoot(&fixes, outcomes)
                }
                Err(response) => response,
            },
        }
    }

    /// Phase 1 of `Troubleshoot`, under the lock: another user's cores, an update or another
    /// batch in progress → `Busy`; a client that is not an administrator → `AuthenticationFailed`
    /// (the daemon then asks through UAC); the Wintun adapter while this service's tunnel is
    /// up → `NotReady`; no troubleshooter → `Unsupported`.
    pub(crate) fn start_troubleshoot(
        &mut self,
        client: &ClientIdentity,
        fixes: &[PrivilegedFix],
    ) -> Result<Arc<dyn Troubleshooter>, ServiceResponse> {
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.user_key != client.user_key)
            || self.updating
            || self.troubleshooting
        {
            return Err(ServiceResponse::Busy);
        }
        let refuse = |code| Err(ServiceResponse::Error { code });
        if !client.administrator {
            return refuse(BrokerErrorCode::AuthenticationFailed);
        }
        if self.tun_on && fixes.contains(&PrivilegedFix::ReinstallWintun) {
            return refuse(BrokerErrorCode::NotReady);
        }
        let Some(troubleshooter) = self.troubleshooter.clone() else {
            return refuse(BrokerErrorCode::Unsupported);
        };
        self.troubleshooting = true;
        Ok(troubleshooter)
    }

    /// Phase 3 of `Troubleshoot`, under the lock again.
    pub(crate) fn finish_troubleshoot(
        &mut self,
        fixes: &[PrivilegedFix],
        outcomes: Vec<FixOutcome>,
    ) -> ServiceResponse {
        self.troubleshooting = false;
        if outcomes.len() == fixes.len() {
            ServiceResponse::Troubleshot { outcomes }
        } else {
            ServiceResponse::Error {
                code: BrokerErrorCode::InvalidRequest,
            }
        }
    }

    /// The connection `connection` closed. Only the current owner's connection stops the
    /// cores: a stale connection of a daemon that was already replaced must not.
    pub(crate) fn client_gone(&mut self, connection: ConnectionId) {
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.connection == connection)
        {
            self.stop_everything();
        }
    }

    /// Periodic health check: a crashed core is restarted once per prepared generation (with
    /// the tunnel switched back on if it was on); a second crash stops both cores, leaves the
    /// tunnel off and reports `NotReady` in `Status` until the next `Prepare`.
    pub(crate) fn tick(&mut self) {
        if self.prepared.is_none() || self.failure.is_some() || self.cores.health().all() {
            return;
        }
        let tunnel = self.tun_on;
        self.tun_on = false;
        if self.crash_restarts >= MAX_CRASH_RESTARTS {
            if tunnel {
                let _ = self.cores.set_tun(false);
            }
            self.cores.stop_all();
            self.failure = Some(BrokerErrorCode::NotReady);
            return;
        }
        self.crash_restarts += 1;
        match self.cores.restart() {
            Ok(()) if tunnel => match self.cores.set_tun(true) {
                Ok(()) => self.tun_on = true,
                Err(code) => {
                    let _ = self.cores.set_tun(false);
                    self.failure = Some(code);
                }
            },
            Ok(()) => {}
            Err(code) => {
                self.cores.stop_all();
                self.failure = Some(code);
            }
        }
    }

    pub(crate) fn stop_everything(&mut self) {
        self.stop_cores();
        self.owner = None;
    }

    /// Phase 1 of `Update` and `StageUpdate`, under the lock: another user's cores → `Busy`, no updater →
    /// `Unsupported`, an update already downloading → `Busy`. Otherwise the updater is taken
    /// out for the download.
    pub(crate) fn start_update(
        &mut self,
        client: &ClientIdentity,
    ) -> Result<PendingUpdate, ServiceResponse> {
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.user_key != client.user_key)
            || self.updating
        {
            return Err(ServiceResponse::Busy);
        }
        let updater = self.updater.take().ok_or(ServiceResponse::Error {
            code: BrokerErrorCode::Unsupported,
        })?;
        self.updating = true;
        Ok(PendingUpdate { updater })
    }

    /// Phase 3 of `Update`, under the lock again: after a successful download (`prepared`),
    /// stop the cores and start the updater, unless another user claimed the cores meanwhile.
    pub(crate) fn finish_update(
        &mut self,
        client: &ClientIdentity,
        version: &str,
        mut pending: PendingUpdate,
        prepared: Result<(), BrokerErrorCode>,
    ) -> ServiceResponse {
        self.updating = false;
        let response = if let Err(code) = prepared {
            ServiceResponse::Error { code }
        } else if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.user_key != client.user_key)
        {
            ServiceResponse::Busy
        } else {
            // Nothing may run from the install directory while the updater swaps it.
            self.stop_everything();
            match pending.updater.begin(client, version) {
                Ok(()) => ServiceResponse::UpdateStarted,
                Err(code) => ServiceResponse::Error { code },
            }
        };
        self.updater = Some(pending.updater);
        response
    }

    /// Phase 3 of `StageUpdate`, under the lock again: the release is downloaded and verified
    /// (or not); either way nothing is stopped, so the owner's tunnel keeps running until the
    /// desktop has disconnected and sends `Update`.
    pub(crate) fn finish_stage(
        &mut self,
        version: &str,
        pending: PendingUpdate,
        prepared: Result<(), BrokerErrorCode>,
    ) -> ServiceResponse {
        self.updating = false;
        self.updater = Some(pending.updater);
        match prepared {
            Ok(()) => ServiceResponse::UpdateStaged {
                version: version.to_owned(),
            },
            Err(code) => ServiceResponse::Error { code },
        }
    }

    fn update(
        &mut self,
        client: &ClientIdentity,
        version: &str,
        stage_only: bool,
    ) -> ServiceResponse {
        let mut pending = match self.start_update(client) {
            Ok(pending) => pending,
            Err(response) => return response,
        };
        let prepared = pending.updater.prepare(client, version);
        if stage_only {
            self.finish_stage(version, pending, prepared)
        } else {
            self.finish_update(client, version, pending, prepared)
        }
    }

    fn claim(&mut self, connection: ConnectionId, client: &ClientIdentity) {
        self.owner = Some(Owner {
            user_key: client.user_key.clone(),
            connection,
        });
    }

    fn prepare(&mut self, client: &ClientIdentity, generation: u64) -> ServiceResponse {
        if let Some((current, endpoint)) = &self.prepared
            && *current == generation
            && self.failure.is_none()
            && self.cores.health().all()
        {
            return ServiceResponse::Prepared {
                generation_id: generation,
                controller: endpoint.clone(),
            };
        }
        let tunnel = self.tun_on;
        self.tun_on = false;
        self.crash_restarts = 0;
        match self.cores.prepare(client, generation) {
            Ok(endpoint) => {
                self.prepared = Some((generation, endpoint.clone()));
                self.failure = None;
                if tunnel {
                    match self.cores.set_tun(true) {
                        Ok(()) => self.tun_on = true,
                        Err(code) => {
                            let _ = self.cores.set_tun(false);
                            self.failure = Some(code);
                        }
                    }
                }
                ServiceResponse::Prepared {
                    generation_id: generation,
                    controller: endpoint,
                }
            }
            Err(code) => {
                self.prepared = None;
                self.failure = Some(code);
                ServiceResponse::Error { code }
            }
        }
    }

    fn set_tun(&mut self, on: bool) -> ServiceResponse {
        if self.prepared.is_none() || self.failure.is_some() {
            return ServiceResponse::Error {
                code: BrokerErrorCode::NotReady,
            };
        }
        if self.tun_on == on {
            return ServiceResponse::TunState { on };
        }
        match self.cores.set_tun(on) {
            Ok(()) => {
                self.tun_on = on;
                ServiceResponse::TunState { on }
            }
            Err(code) => {
                if on {
                    let _ = self.cores.set_tun(false);
                    self.tun_on = false;
                }
                // A failed switch-off may have left the tunnel up: unless the cores can confirm
                // it is off (e.g. only the mixed-port recheck failed), `tun_on` stays true so the
                // next `SetTun{on:false}` retries instead of short-circuiting.
                if !on {
                    self.tun_on = self.cores.tun_is_on().unwrap_or(true);
                }
                ServiceResponse::Error { code }
            }
        }
    }

    fn status(&mut self) -> ServiceResponse {
        let health = if self.prepared.is_some() {
            self.cores.health()
        } else {
            CoreHealth::default()
        };
        let state = |running: bool| {
            if running {
                ElevatedRuntimeState::Ready
            } else {
                ElevatedRuntimeState::Stopped
            }
        };
        ServiceResponse::Status {
            generation_id: self.prepared.as_ref().map(|(generation, _)| *generation),
            xray: state(health.xray),
            mihomo: state(health.mihomo),
            tun_on: self.tun_on,
            failure: self.failure,
        }
    }

    fn stop_cores(&mut self) {
        if self.tun_on {
            let _ = self.cores.set_tun(false);
        }
        self.cores.stop_all();
        self.prepared = None;
        self.tun_on = false;
        self.crash_restarts = 0;
        self.failure = None;
    }
}

/// Runs one command for a server connection thread. `Update` and `StageUpdate` download and
/// verify without holding the lock (and `Troubleshoot` runs its fixes without it), so the owner's `Status`/`SetTun`, crash restarts, a dropped
/// owner and a service stop are all served meanwhile; every other command runs under the lock.
pub(crate) fn execute_shared<C: Cores>(
    service: &Mutex<ServiceLoop<C>>,
    connection: ConnectionId,
    client: &ClientIdentity,
    command: ServiceCommand,
) -> ServiceResponse {
    let lock = || service.lock().unwrap_or_else(PoisonError::into_inner);
    let (version, stage_only) = match command {
        ServiceCommand::Update { version } => (version, false),
        ServiceCommand::StageUpdate { version } => (version, true),
        // Fixes run tools and wait for services to stop: outside the lock, like a download.
        ServiceCommand::Troubleshoot { fixes } => {
            let troubleshooter = match lock().start_troubleshoot(client, &fixes) {
                Ok(troubleshooter) => troubleshooter,
                Err(response) => return response,
            };
            let outcomes = troubleshooter.run(&fixes);
            return lock().finish_troubleshoot(&fixes, outcomes);
        }
        command => return lock().execute(connection, client, command),
    };
    let mut pending = match lock().start_update(client) {
        Ok(pending) => pending,
        Err(response) => return response,
    };
    let prepared = pending.updater.prepare(client, &version);
    if stage_only {
        lock().finish_stage(&version, pending, prepared)
    } else {
        lock().finish_update(client, &version, pending, prepared)
    }
}
