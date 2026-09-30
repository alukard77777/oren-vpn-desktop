//! Platform-neutral lifecycle of the always-ready service. The platform server resolves who is
//! on each connection; this loop decides what that client may do and drives the cores.

use std::path::PathBuf;

use multicore_core::elevation_protocol::{BrokerErrorCode, ElevatedRuntimeState};
use multicore_core::service_protocol::{ControllerEndpoint, ServiceCommand, ServiceResponse};

/// Who is on the other end of one connection, as established by the platform server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClientIdentity {
    /// Same user in the same session: Windows `"<session>:<sid>"`, Unix `"uid:<uid>"`.
    pub(crate) user_key: String,
    /// The user's MultiCore data directory, resolved from the client's token / uid.
    pub(crate) data_root: PathBuf,
    /// Unix: the uid whose ownership the runtime resolver enforces. Windows: 0.
    pub(crate) uid: u32,
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
    fn health(&mut self) -> CoreHealth;
    fn stop_all(&mut self);
    fn diagnostics(&mut self) -> ServiceResponse;
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
        }
    }

    pub(crate) fn cores_mut(&mut self) -> &mut C {
        &mut self.cores
    }

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
            ServiceCommand::Shutdown => {
                self.stop_everything();
                ServiceResponse::ShuttingDown
            }
            ServiceCommand::Update { .. } => ServiceResponse::Error {
                code: BrokerErrorCode::Unsupported,
            },
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
                // A failed switch-off may have left the tunnel up: `tun_on` stays true so the
                // next `SetTun{on:false}` retries instead of short-circuiting.
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
