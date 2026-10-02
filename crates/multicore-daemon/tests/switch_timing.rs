//! Timing harness for a subscription switch in always-ready (service) mode.
//!
//! Run with:
//! `cargo test -p multicore-daemon --release --test switch_timing -- --ignored --nocapture`
//!
//! Everything daemon-side is real (profile store on disk, snapshot parsing, runtime staging,
//! the switch orchestration and the `subscription_switch` timing event); the service is
//! simulated with fixed delays for its three round trips (`SetTun{off}`, `Prepare`,
//! `SetTun{on}`), so the numbers show what the daemon adds on top of the service.

use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use multicore_core::{
    Engine, FetchError, HttpClient, HttpResponse, MihomoRuntimeControl, ProcessController,
    ProfileId, ProfileStore, Snapshot, SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_XRAY,
    stage_runtime_with_mihomo_control,
};
use multicore_daemon::{
    Backend, BackendError, ConnectOptions, CoreBackend, CoresReadiness, ImportSubscriptionRequest,
    MihomoSelector, PreparedController, SelectorError, options_controller_factory,
};

/// Simulated service round trips (typical values from `service.log` phase lines).
const SERVICE_TUN_OFF: Duration = Duration::from_millis(250);
const SERVICE_PREPARE: Duration = Duration::from_millis(1500);
const SERVICE_TUN_ON: Duration = Duration::from_millis(1200);

const PROFILES: usize = 6;
const PROXIES: usize = 400;
const GROUPS: usize = 24;
const RULES: usize = 6000;
const SWITCHES: usize = 6;

/// Serves `large_profile(seed)` for `https://p<seed>.invalid/sub`, the same bytes on every
/// fetch (a refresh that only renews the traffic counters).
struct ProviderHttp;

impl HttpClient for ProviderHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            let seed = url
                .strip_prefix("https://p")
                .and_then(|rest| rest.split('.').next())
                .and_then(|seed| seed.parse::<usize>().ok())
                .ok_or(FetchError::Network)?;
            let yaml = large_profile(seed);
            let body = match user_agent {
                UA_NATIVE => format!("[{},{{}}]", serde_json::to_string(&yaml).unwrap()),
                UA_MIHOMO => yaml,
                UA_XRAY => "{}".to_owned(),
                _ => return Err(FetchError::Network),
            };
            Ok(HttpResponse::new(
                200,
                body.into_bytes(),
                [(
                    "subscription-userinfo",
                    "upload=1; download=2; total=1073741824",
                )],
            ))
        })
    }
}

static PREPARES: AtomicUsize = AtomicUsize::new(0);

struct SimulatedService;

impl ProcessController for SimulatedService {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Xray {
                PREPARES.fetch_add(1, Ordering::SeqCst);
            }
            tokio::time::sleep(match engine {
                Engine::Xray => SERVICE_PREPARE,
                Engine::Mihomo => SERVICE_TUN_ON,
            })
            .await;
            Ok(())
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            if engine == Engine::Mihomo {
                tokio::time::sleep(SERVICE_TUN_OFF).await;
            }
            Ok(())
        })
    }
}

struct Ready;

impl CoresReadiness for Ready {
    fn cores_ready(&self) -> bool {
        true
    }
    fn service_available(&self) -> bool {
        true
    }
    fn connection_lost(&self) -> bool {
        false
    }
    fn release(&self) {}
}

struct Selector;

#[async_trait]
impl MihomoSelector for Selector {
    async fn select(&self, _group: &str, _node: &str) -> Result<(), SelectorError> {
        Ok(())
    }
    async fn probe_latency(&self, _: &str) -> Result<u64, multicore_daemon::LatencyProbeError> {
        Ok(42)
    }
    async fn proxy_now(&self, _raw_name: &str) -> Result<Option<String>, SelectorError> {
        Ok(None)
    }
}

/// A provider-sized Mihomo document: `PROXIES` nodes of four protocols, `GROUPS` groups and
/// `RULES` rules.
fn large_profile(seed: usize) -> String {
    let mut yaml = String::from("mixed-port: 7890\nmode: rule\nproxies:\n");
    for index in 0..PROXIES {
        let uuid = format!(
            "{:08x}-{:04x}-4abc-8def-{:012x}",
            seed,
            index,
            index * 7 + 1
        );
        let line = match index % 4 {
            0 => format!(
                "  - {{name: \"Node {seed}-{index}\", type: trojan, server: n{index}.p{seed}.example.com, port: 443, password: pw{index}, sni: n{index}.p{seed}.example.com, udp: true}}\n"
            ),
            1 => format!(
                "  - {{name: \"Node {seed}-{index}\", type: ss, server: n{index}.p{seed}.example.com, port: 8388, cipher: aes-128-gcm, password: pw{index}}}\n"
            ),
            2 => format!(
                "  - {{name: \"Node {seed}-{index}\", type: vmess, server: n{index}.p{seed}.example.com, port: 443, uuid: {uuid}, alterId: 0, cipher: auto, tls: true}}\n"
            ),
            _ => format!(
                "  - {{name: \"Node {seed}-{index}\", type: vless, server: n{index}.p{seed}.example.com, port: 443, uuid: {uuid}, tls: true, servername: n{index}.p{seed}.example.com, network: tcp}}\n"
            ),
        };
        yaml.push_str(&line);
    }
    yaml.push_str("proxy-groups:\n");
    for group in 0..GROUPS {
        let members = (0..PROXIES)
            .filter(|index| group == 0 || index % GROUPS == group)
            .map(|index| format!("\"Node {seed}-{index}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let kind = if group % 3 == 1 { "url-test" } else { "select" };
        let extra = if kind == "url-test" {
            ", url: \"https://www.gstatic.com/generate_204\", interval: 300"
        } else {
            ""
        };
        yaml.push_str(&format!(
            "  - {{name: \"Group {group}\", type: {kind}, proxies: [{members}]{extra}}}\n"
        ));
    }
    yaml.push_str("rules:\n");
    for rule in 0..RULES {
        yaml.push_str(&format!(
            "  - DOMAIN-SUFFIX,site{rule}.p{seed}.example.org,Group {}\n",
            rule % GROUPS
        ));
    }
    yaml.push_str("  - MATCH,Group 0\n");
    yaml
}

type FactoryFn = Box<
    dyn Fn(&Snapshot, ConnectOptions) -> Result<PreparedController<SimulatedService>, BackendError>
        + Send
        + Sync,
>;

fn millis(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing harness; run explicitly with --release --nocapture"]
async fn switch_timing_harness() {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();

    // Real staging into a runtime root, reused for an unchanged snapshot like `WarmStages`.
    let runtime_root = root.path().join("runtime");
    let control = MihomoRuntimeControl::new(
        "127.0.0.1:19090".parse::<SocketAddr>().unwrap(),
        "0123456789abcdef0123456789abcdef".to_owned(),
    )
    .unwrap();
    let staged: Arc<Mutex<HashMap<String, ()>>> = Arc::default();
    let factory: FactoryFn = Box::new(move |snapshot: &Snapshot, _options| {
        let key = snapshot.mihomo.raw_yaml().to_owned();
        let controller = Arc::new(SimulatedService);
        if staged.lock().unwrap().insert(key, ()).is_some() {
            return Ok(PreparedController::reuse(controller));
        }
        let runtime = stage_runtime_with_mihomo_control(snapshot, &runtime_root, &control)
            .map_err(|_| BackendError::new())?;
        Ok(PreparedController::with_runtime(controller, runtime))
    });
    let backend = Arc::new(
        CoreBackend::new_with_connect_options(
            store,
            SubscriptionFetcher::new(ProviderHttp),
            options_controller_factory(factory),
            Arc::new(Selector),
            None,
        )
        .unwrap(),
    );
    let mut ids = Vec::new();
    for seed in 0..PROFILES {
        let status = backend
            .import_subscription(ImportSubscriptionRequest {
                url: format!("https://p{seed}.invalid/sub"),
            })
            .await
            .unwrap();
        ids.push(ProfileId::parse(&status.imported_profile_id.unwrap()).unwrap());
    }
    Backend::activate_subscription(&*backend, ids[0].clone())
        .await
        .unwrap();
    backend.enable_always_ready(Arc::new(Ready)).await;
    backend.warm_up().await.unwrap();
    backend.connect().await.unwrap();

    println!(
        "simulated service: tun off {} ms, prepare {} ms, tun on {} ms ({} ms total)",
        SERVICE_TUN_OFF.as_millis(),
        SERVICE_PREPARE.as_millis(),
        SERVICE_TUN_ON.as_millis(),
        (SERVICE_TUN_OFF + SERVICE_PREPARE + SERVICE_TUN_ON).as_millis()
    );
    let mut totals = Vec::new();
    let mut lists = Vec::new();
    let mut catalogs = Vec::new();
    let mut refreshes = Vec::new();
    let mut refresh_prepares = 0;
    for round in 0..SWITCHES {
        let target = ids[(round + 1) % PROFILES].clone();
        let started = Instant::now();
        let status = Backend::activate_subscription(&*backend, target.clone())
            .await
            .unwrap();
        let switch_ms = started.elapsed().as_millis();
        assert_eq!(status.active_profile_id.as_deref(), Some(target.as_str()));
        let started = Instant::now();
        Backend::list_subscriptions(&*backend).await.unwrap();
        let list_ms = started.elapsed().as_millis();
        let started = Instant::now();
        Backend::catalog(&*backend).await.unwrap();
        let catalog_ms = started.elapsed().as_millis();
        // The desktop refreshes a stale subscription right after switching to it.
        let prepares = PREPARES.load(Ordering::SeqCst);
        let started = Instant::now();
        Backend::refresh_subscription(&*backend).await.unwrap();
        let refresh_ms = started.elapsed().as_millis();
        let reconnected = PREPARES.load(Ordering::SeqCst) != prepares;
        refresh_prepares += usize::from(reconnected);
        let events = backend.events_after(0).await.unwrap();
        let details = events
            .iter()
            .rev()
            .find(|event| {
                event
                    .details
                    .as_ref()
                    .is_some_and(|details| details.get("switch_ms").is_some())
            })
            .and_then(|event| event.details.clone())
            .unwrap_or_default();
        println!(
            "switch {round}: {switch_ms} ms (pause {} / load {} / prepare {} / commit {} / resume {}), then list {list_ms} ms, catalog {catalog_ms} ms; unchanged refresh {refresh_ms} ms{}",
            millis(&details, "pause_ms"),
            millis(&details, "load_ms"),
            millis(&details, "prepare_ms"),
            millis(&details, "commit_ms"),
            millis(&details, "resume_ms"),
            if reconnected { " (reconnected)" } else { "" },
        );
        refreshes.push(refresh_ms);
        totals.push(switch_ms);
        lists.push(list_ms);
        catalogs.push(catalog_ms);
    }
    let average = |values: &[u128]| values.iter().sum::<u128>() / values.len() as u128;
    println!(
        "average: switch {} ms, list {} ms, catalog {} ms, unchanged refresh {} ms ({refresh_prepares}/{SWITCHES} reconnected); daemon overhead over the simulated service {} ms",
        average(&totals),
        average(&lists),
        average(&catalogs),
        average(&refreshes),
        average(&totals)
            .saturating_sub((SERVICE_TUN_OFF + SERVICE_PREPARE + SERVICE_TUN_ON).as_millis())
    );
}
