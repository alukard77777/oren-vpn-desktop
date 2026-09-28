mod device_identity;
pub mod elevation_protocol;
mod event;
mod fetch;
mod input;
mod network;
mod profile;
mod service_logo;
mod sidecar;
mod snapshot;
mod state;
mod store_root;
mod supervisor;
#[cfg(unix)]
mod unix_group;
#[cfg(windows)]
mod windows_job;

pub use device_identity::{DeviceIdentity, DeviceIdentityError, derive_incy_hwid, is_valid_hwid};
pub use event::{Component, Event, Severity};
pub use fetch::{
    AttemptOutcome, FetchDiagnostics, FetchError, HttpClient, HttpResponse, ReqwestHttpClient,
    SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_SERVICE_LOGO, UA_XRAY,
};
pub use input::{
    ConfigError, MAX_CONFIG_BYTES, MihomoConfig, MihomoSocksMapping, ProxyGroup, XrayConfig,
    parse_mihomo, parse_xray,
};
pub use network::default_ipv4_interface;
pub use profile::{
    LoadedProfile, MAX_PROFILE_INDEX_BYTES, MAX_PROFILES, MAX_SELECTIONS, MAX_SELECTIONS_BYTES,
    ProfileError, ProfileId, ProfileStore, ProfileSummary,
};
pub use service_logo::{
    MAX_SERVICE_LOGO_BYTES, MAX_SERVICE_LOGO_OUTPUT_BYTES, normalize_service_logo,
};
pub use sidecar::{
    CORE_READINESS_TIMEOUT, CommandSpec, CoreLogBuffer, CoreLogRecord, DiagnosticStream,
    ManagedChild, ProcessDiagnostics, ProcessError, ProcessLauncher, RuntimeCheckState,
    RuntimePaths, SidecarProcessController, TokioManagedChild, TokioProcessLauncher,
};
pub use snapshot::{
    AtomicSnapshot, MihomoRuntimeControl, PersistenceError, PersistentSnapshotStore,
    PublishedRuntime, ServiceLogoLease, Snapshot, SnapshotCommit, SnapshotSink, StagedRuntime,
    SubscriptionInfo, publish_runtime, publish_runtime_with_mihomo_control,
    stage_runtime_with_mihomo_control, xray_outbound_server_domains,
};
pub use state::{ConnectionState, StateError, StateMachine};
pub use supervisor::{Engine, ProcessController, StartFailureCause, Supervisor, SupervisorError};
#[cfg(unix)]
pub use unix_group::{UnixGroupLauncher, UnixGroupManagedChild};
#[cfg(windows)]
pub use windows_job::{WindowsJobLauncher, WindowsJobManagedChild};
