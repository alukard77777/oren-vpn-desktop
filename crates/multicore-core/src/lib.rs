mod device_identity;
pub mod elevation_protocol;
mod event;
mod fetch;
mod input;
mod network;
mod profile;
mod service_logo;
pub mod service_protocol;
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
    ConfigError, IDLE_XRAY_CONFIG, IDLE_XRAY_OUTBOUND_TAG, MAX_CONFIG_BYTES, MihomoConfig,
    MihomoSocksMapping, ProxyGroup, XrayConfig, parse_mihomo, parse_xray,
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

static CRYPTO_PROVIDER_INSTALL: std::sync::Once = std::sync::Once::new();

/// Installs `ring` as the process-wide default rustls crypto provider.
/// Idempotent and safe to call from multiple sites or threads - only the
/// first call does anything, later ones are no-ops.
///
/// This workspace carries two reqwest majors across its binaries (0.12 in
/// the desktop app, 0.13 here), and reqwest 0.13's plain `rustls` feature
/// pulls in `aws-lc-rs` by default. Left alone, a binary that links both
/// (the desktop app does, since it depends on this crate too) ends up with
/// *both* `aws-lc-rs` and `ring` in its dependency graph, which rustls
/// resolves ambiguously depending on install order - a documented source
/// of inconsistent crypto-provider initialization. This workspace instead
/// pins `rustls-no-provider` everywhere and installs `ring` explicitly
/// here, matching a working precedent in the same problem domain (Clash
/// Verge Rev does the same). Must run before any TLS connection is
/// attempted; every `reqwest::Client` this crate builds calls this first.
pub fn ensure_crypto_provider_installed() {
    CRYPTO_PROVIDER_INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
