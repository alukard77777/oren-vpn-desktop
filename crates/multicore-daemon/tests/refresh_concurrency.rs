//! A subscription refresh downloads without the daemon's operation lock: route selections
//! stay instant while the provider answers, and the refreshed subscription keeps them.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use multicore_core::{
    Engine, FetchError, HttpClient, HttpResponse, ProcessController, ProfileStore,
    SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_XRAY,
};
use multicore_daemon::{Backend, CoreBackend, ImportSubscriptionRequest, SelectionRequest};
use tokio::sync::Notify;

const FIRST: &str = "proxies:\n  - {name: A, type: socks5}\n  - {name: B, type: socks5}\nproxy-groups:\n  - name: Main\n    type: select\n    proxies: [A, B]\n";
const REFRESHED: &str = "proxies:\n  - {name: A, type: socks5}\n  - {name: B, type: socks5}\n  - {name: C, type: socks5}\nproxy-groups:\n  - name: Main\n    type: select\n    proxies: [A, B, C]\n";

/// Serves `FIRST` until the first download is done, then `REFRESHED`; while `hold` is set,
/// a subscription request waits for `release` (a provider that takes its time).
#[derive(Clone, Default)]
struct SlowProvider {
    served: Arc<AtomicBool>,
    hold: Arc<AtomicBool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl HttpClient for SlowProvider {
    fn get<'a>(
        &'a self,
        _url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            if self.hold.load(Ordering::SeqCst) && user_agent == UA_MIHOMO {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let empty = std::iter::empty::<(&str, &str)>();
            Ok(match user_agent {
                UA_NATIVE => HttpResponse::new(404, Vec::new(), empty),
                UA_MIHOMO => {
                    let refreshed = self.served.swap(true, Ordering::SeqCst);
                    let body = if refreshed { REFRESHED } else { FIRST };
                    HttpResponse::new(200, body.as_bytes().to_vec(), empty)
                }
                UA_XRAY => HttpResponse::new(200, b"{}".to_vec(), empty),
                _ => return Err(FetchError::Network),
            })
        })
    }
}

struct NoopController;

impl ProcessController for NoopController {
    type Error = ();

    fn start<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn stop<'a>(
        &'a self,
        _engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selections_stay_instant_during_a_refresh_and_survive_its_commit() {
    let directory = tempfile::tempdir().unwrap();
    let provider = SlowProvider::default();
    let backend = Arc::new(
        CoreBackend::new(
            ProfileStore::open(directory.path().join("profiles"), None).unwrap(),
            SubscriptionFetcher::new(provider.clone()),
            |_snapshot| Ok(Arc::new(NoopController)),
        )
        .unwrap(),
    );
    backend
        .import_subscription(ImportSubscriptionRequest {
            url: "https://provider.invalid/sub".into(),
        })
        .await
        .unwrap();
    let before = backend.catalog().await.unwrap();

    // The refresh waits on a slow provider...
    provider.hold.store(true, Ordering::SeqCst);
    let refreshing = {
        let backend = backend.clone();
        tokio::spawn(async move { Backend::refresh_subscription(backend.as_ref()).await })
    };
    tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
        .await
        .expect("the refresh is downloading");

    // ...and a route choice meanwhile is applied at once instead of waiting for it.
    let selected = tokio::time::timeout(
        Duration::from_secs(1),
        backend.select(SelectionRequest {
            revision: before.revision,
            group_id: before.groups[0].id.clone(),
            node_id: before.groups[0].nodes[1].id.clone(),
        }),
    )
    .await
    .expect("a selection does not wait for the refresh's download")
    .expect("selection applied");
    assert!(selected.groups[0].nodes[1].selected);

    provider.hold.store(false, Ordering::SeqCst);
    provider.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), refreshing)
        .await
        .expect("the refresh finishes")
        .unwrap()
        .expect("refresh committed");

    // The refreshed subscription (a new revision, one more server) keeps that choice.
    let after = backend.catalog().await.unwrap();
    assert_ne!(after.revision, before.revision);
    let labels: Vec<_> = after.groups[0]
        .nodes
        .iter()
        .map(|node| (node.label.as_str(), node.selected))
        .collect();
    assert_eq!(labels, [("A", false), ("B", true), ("C", false)]);
    assert_eq!(
        backend.status().await.unwrap().current_node.as_deref(),
        Some("B")
    );
}
