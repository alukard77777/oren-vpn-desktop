use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use multicore_core::{MAX_PROFILES, ProfileError, ProfileStore, Snapshot};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn snapshot(source: &str, name: &str) -> Arc<Snapshot> {
    let mihomo = format!(
        "proxies:\n  - name: {name}\n    type: socks5\n    server: 127.0.0.1\n    port: 31001\nproxy-groups:\n  - name: Proxy\n    type: select\n    proxies: [{name}]\nrules:\n  - MATCH,Proxy\n"
    );
    Arc::new(
        Snapshot::parse(mihomo.as_bytes(), br#"{"outbounds":[]}"#)
            .unwrap()
            .with_subscription_source(source, None, 1_700_000_000)
            .unwrap(),
    )
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "multicore-profile-store-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn profiles(&self) -> PathBuf {
        self.0.join("profiles")
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn ids_are_unique_lowercase_v4_uuids_and_index_keeps_urls_secret() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    let first = store
        .create_profile(snapshot("https://one.invalid/private-a", "One"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://two.invalid/private-b", "Two"))
        .unwrap();

    assert_ne!(first, second);
    for id in [&first, &second] {
        let text = id.as_str();
        assert_eq!(text.len(), 36);
        assert_eq!(&text[14..15], "4");
        assert!(matches!(&text[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(text, text.to_ascii_lowercase());
    }

    store.activate(&first).unwrap();
    assert_eq!(store.active_id(), Some(first.clone()));
    let index = fs::read_to_string(root.profiles().join("index.json")).unwrap();
    assert!(!index.contains("https://"));
    assert!(!index.contains("private-a"));
    assert!(!index.contains("private-b"));
    assert_eq!(store.list().unwrap().len(), 2);
    assert_eq!(index.matches("active_id").count(), 1);
}

#[test]
fn seventeenth_profile_is_rejected_without_changing_index() {
    let root = TempDirectory::new();
    let store = ProfileStore::open(root.profiles(), None).unwrap();
    for index in 0..MAX_PROFILES {
        store
            .create_profile(snapshot(
                &format!("https://{index}.invalid/sub"),
                &format!("Node{index}"),
            ))
            .unwrap();
    }
    let before = fs::read(root.profiles().join("index.json")).unwrap();
    assert!(matches!(
        store.create_profile(snapshot("https://overflow.invalid/sub", "Overflow")),
        Err(ProfileError::LimitReached)
    ));
    assert_eq!(
        fs::read(root.profiles().join("index.json")).unwrap(),
        before
    );
}

#[test]
fn create_replace_activate_delete_and_exact_lookup_stay_isolated() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let first = store
        .create_profile(snapshot("https://same.invalid/sub?a=1", "First"))
        .unwrap();
    let second = store
        .create_profile(snapshot("https://same.invalid/sub?a=2", "Second"))
        .unwrap();

    assert_eq!(
        store
            .find_exact_source("https://same.invalid/sub?a=1")
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        store.find_exact_source("https://same.invalid/sub").unwrap(),
        None
    );
    assert_eq!(store.active_id(), Some(second.clone()));
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Second"]
    );

    let generation = store
        .replace_profile(
            &first,
            snapshot("https://same.invalid/sub?a=1", "FirstUpdated"),
        )
        .unwrap();
    assert_eq!(generation, 2);
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Second"]
    );
    assert_eq!(
        store.activate(&first).unwrap().mihomo.proxy_names(),
        &["FirstUpdated"]
    );

    assert_eq!(store.delete(&first).unwrap(), Some(second.clone()));
    assert_eq!(store.active_id(), Some(second.clone()));
    assert!(!profiles.join(first.as_str()).exists());
    assert!(profiles.join(second.as_str()).exists());
    assert_eq!(store.delete(&second).unwrap(), None);
    assert_eq!(store.active_id(), None);
}

#[test]
fn corrupt_child_is_unavailable_and_failed_activation_keeps_index_active() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let good = store
        .create_profile(snapshot("https://good.invalid/sub", "Good"))
        .unwrap();
    let corrupt = store
        .create_profile(snapshot("https://bad.invalid/sub", "Bad"))
        .unwrap();
    store.activate(&good).unwrap();
    drop(store);

    let generation = profiles
        .join(corrupt.as_str())
        .join("snapshot-00000000000000000001");
    fs::write(generation.join("xray.json"), b"{broken").unwrap();

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    let summaries = reopened.list().unwrap();
    assert!(
        summaries
            .iter()
            .any(|item| item.id == good && item.available)
    );
    assert!(
        summaries
            .iter()
            .any(|item| item.id == corrupt && !item.available)
    );
    let before = fs::read(profiles.join("index.json")).unwrap();
    assert!(matches!(
        reopened.activate(&corrupt),
        Err(ProfileError::Unavailable(_))
    ));
    assert_eq!(reopened.active_id(), Some(good));
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
}

#[test]
fn failed_replace_preserves_last_good_snapshot_and_active_index() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    let store = ProfileStore::open(&profiles, None).unwrap();
    let id = store
        .create_profile(snapshot("https://stable.invalid/sub", "Stable"))
        .unwrap();
    let before = fs::read(profiles.join("index.json")).unwrap();
    let blocked_staging = profiles.join(id.as_str()).join(format!(
        ".snapshot-00000000000000000002.staging-{}",
        std::process::id()
    ));
    fs::create_dir(&blocked_staging).unwrap();

    assert!(
        store
            .replace_profile(&id, snapshot("https://stable.invalid/sub", "Rejected"))
            .is_err()
    );
    assert_eq!(fs::read(profiles.join("index.json")).unwrap(), before);
    assert_eq!(
        store
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Stable"]
    );
    drop(store);

    let reopened = ProfileStore::open(&profiles, None).unwrap();
    assert_eq!(
        reopened
            .active_snapshot()
            .unwrap()
            .unwrap()
            .2
            .mihomo
            .proxy_names(),
        &["Stable"]
    );
}

#[test]
fn invalid_deserialized_id_is_rejected_before_any_child_path_is_used() {
    let root = TempDirectory::new();
    let profiles = root.profiles();
    fs::create_dir(&profiles).unwrap();
    fs::write(
        profiles.join("index.json"),
        br#"{"schema_version":1,"profiles":[{"id":"../escape","created_at_unix":1}],"active_id":"../escape"}"#,
    )
    .unwrap();

    assert!(matches!(
        ProfileStore::open(profiles, None),
        Err(ProfileError::InvalidIndex(_))
    ));
}
