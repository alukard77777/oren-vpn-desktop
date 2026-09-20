use std::{
    collections::HashSet,
    ffi::OsStr,
    fmt, fs, io,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    PersistenceError, PersistentSnapshotStore, Snapshot,
    store_root::{DirectoryCapability, StoreLock},
};

pub const MAX_PROFILES: usize = 16;
pub const MAX_PROFILE_INDEX_BYTES: u64 = 16 * 1024;
const INDEX_SCHEMA_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const INDEX_TEMP_FILE: &str = "index.json.tmp";

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        if is_valid_profile_id(value) {
            Ok(Self(value.to_owned()))
        } else {
            Err(ProfileError::InvalidProfileId)
        }
    }

    fn generate() -> Result<Self, ProfileError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let value = format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
            bytes[9],
            bytes[10],
            bytes[11],
            bytes[12],
            bytes[13],
            bytes[14],
            bytes[15]
        );
        Ok(Self(value))
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if is_valid_profile_id(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("invalid profile ID"))
        }
    }
}

impl TryFrom<&str> for ProfileId {
    type Error = ProfileError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: ProfileId,
    pub created_at_unix: u64,
    pub active: bool,
    pub available: bool,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("profile storage operation failed")]
    Io(#[from] io::Error),
    #[error("profile snapshot operation failed")]
    Snapshot(#[from] PersistenceError),
    #[error("profile index is invalid: {0}")]
    InvalidIndex(String),
    #[error("profile ID is invalid")]
    InvalidProfileId,
    #[error("profile limit reached")]
    LimitReached,
    #[error("profile store is already open")]
    Locked,
    #[error("profile not found")]
    NotFound(ProfileId),
    #[error("profile is unavailable")]
    Unavailable(ProfileId),
    #[error("profile was deleted but its files require cleanup")]
    CleanupDebt {
        id: ProfileId,
        #[source]
        source: io::Error,
    },
    #[error("profile index mutation committed but durability maintenance is required")]
    IndexMaintenanceDebt {
        id: ProfileId,
        #[source]
        source: io::Error,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProfileIndex {
    schema_version: u32,
    profiles: Vec<ProfileIndexEntry>,
    active_id: Option<ProfileId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProfileIndexEntry {
    id: ProfileId,
    created_at_unix: u64,
}

#[derive(Debug)]
pub struct ProfileStore {
    root: DirectoryCapability,
    _lock: StoreLock,
    index: RwLock<ProfileIndex>,
    #[cfg(test)]
    index_fault: std::sync::atomic::AtomicU8,
    #[cfg(test)]
    load_fault: std::sync::Mutex<Option<ProfileId>>,
}

enum IndexCommit {
    Clean,
    MaintenanceDebt(io::Error),
}

impl ProfileStore {
    pub fn open(
        root: impl Into<PathBuf>,
        _legacy_root: Option<PathBuf>,
    ) -> Result<Self, ProfileError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let root = DirectoryCapability::open(&root).map_err(map_root_open_error)?;
        let lock = root.lock_exclusive().map_err(map_lock_error)?;
        let index = match root.file_metadata(OsStr::new(INDEX_FILE))? {
            Some((regular, len)) => {
                if !regular {
                    return Err(ProfileError::InvalidIndex(
                        "index is not a regular file".to_owned(),
                    ));
                }
                if len > MAX_PROFILE_INDEX_BYTES {
                    return Err(ProfileError::InvalidIndex(
                        "index exceeds size limit".to_owned(),
                    ));
                }
                let bytes = root.read_file(OsStr::new(INDEX_FILE), MAX_PROFILE_INDEX_BYTES)?;
                let index: ProfileIndex = serde_json::from_slice(&bytes)
                    .map_err(|error| ProfileError::InvalidIndex(error.to_string()))?;
                validate_index(&index)?;
                index
            }
            None => ProfileIndex {
                schema_version: INDEX_SCHEMA_VERSION,
                profiles: Vec::new(),
                active_id: None,
            },
        };
        cleanup_unindexed_profile_directories(&root, &index)?;
        Ok(Self {
            root,
            _lock: lock,
            index: RwLock::new(index),
            #[cfg(test)]
            index_fault: std::sync::atomic::AtomicU8::new(0),
            #[cfg(test)]
            load_fault: std::sync::Mutex::new(None),
        })
    }

    pub fn list(&self) -> Result<Vec<ProfileSummary>, ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        Ok(index
            .profiles
            .iter()
            .map(|entry| ProfileSummary {
                id: entry.id.clone(),
                created_at_unix: entry.created_at_unix,
                active: index.active_id.as_ref() == Some(&entry.id),
                available: self.load_profile(&entry.id).is_ok(),
            })
            .collect())
    }

    pub fn active_id(&self) -> Option<ProfileId> {
        self.index
            .read()
            .expect("profile index lock poisoned")
            .active_id
            .clone()
    }

    pub fn active_snapshot(&self) -> Result<Option<(ProfileId, u64, Arc<Snapshot>)>, ProfileError> {
        let Some(id) = self.active_id() else {
            return Ok(None);
        };
        let (generation, snapshot) = self.load_profile(&id)?;
        Ok(Some((id, generation, snapshot)))
    }

    pub fn create_profile(&self, snapshot: Arc<Snapshot>) -> Result<ProfileId, ProfileError> {
        let mut index = self.index.write().expect("profile index lock poisoned");
        if occupied_profile_slots(&self.root, &index)? >= MAX_PROFILES {
            return Err(ProfileError::LimitReached);
        }
        let id = loop {
            let candidate = ProfileId::generate()?;
            if !index.profiles.iter().any(|entry| entry.id == candidate)
                && self
                    .root
                    .file_metadata(OsStr::new(candidate.as_str()))?
                    .is_none()
            {
                break candidate;
            }
        };
        let profile = self.root.create_child(OsStr::new(id.as_str()))?;
        let result = (|| {
            let store = PersistentSnapshotStore::open_capability(profile)?;
            store.commit((*snapshot).clone())?;
            drop(store);
            self.load_profile(&id)?;
            Ok::<_, ProfileError>(())
        })();
        if let Err(error) = result {
            if let Err(source) = self.remove_profile_directory(&id) {
                return Err(ProfileError::CleanupDebt { id, source });
            }
            return Err(error);
        }

        let mut proposed = index.clone();
        proposed.profiles.push(ProfileIndexEntry {
            id: id.clone(),
            created_at_unix: unix_timestamp(),
        });
        proposed.active_id = Some(id.clone());
        match self.persist_index(&proposed) {
            Ok(IndexCommit::Clean) => {
                *index = proposed;
                Ok(id)
            }
            Ok(IndexCommit::MaintenanceDebt(source)) => {
                *index = proposed;
                Err(ProfileError::IndexMaintenanceDebt { id, source })
            }
            Err(error) => match self.remove_profile_directory(&id) {
                Ok(()) => Err(error),
                Err(source) => Err(ProfileError::CleanupDebt { id, source }),
            },
        }
    }

    pub fn replace_profile(
        &self,
        id: &ProfileId,
        snapshot: Arc<Snapshot>,
    ) -> Result<u64, ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let profile = self.open_profile(id)?;
        let store = PersistentSnapshotStore::open_capability(profile)?;
        let (generation, _) = store.commit_with_generation((*snapshot).clone())?;
        drop(store);
        let (reloaded_generation, _) = self.load_profile(id)?;
        if reloaded_generation != generation {
            return Err(ProfileError::Unavailable(id.clone()));
        }
        Ok(generation)
    }

    pub fn activate(&self, id: &ProfileId) -> Result<Arc<Snapshot>, ProfileError> {
        let mut index = self.index.write().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let (_, snapshot) = self.load_profile(id)?;
        if index.active_id.as_ref() != Some(id) {
            let mut proposed = index.clone();
            proposed.active_id = Some(id.clone());
            match self.persist_index(&proposed)? {
                IndexCommit::Clean => *index = proposed,
                IndexCommit::MaintenanceDebt(source) => {
                    *index = proposed;
                    return Err(ProfileError::IndexMaintenanceDebt {
                        id: id.clone(),
                        source,
                    });
                }
            }
        }
        Ok(snapshot)
    }

    pub fn delete(&self, id: &ProfileId) -> Result<Option<ProfileId>, ProfileError> {
        let mut index = self.index.write().expect("profile index lock poisoned");
        let position = self.require_entry(&index, id)?;
        let mut proposed = index.clone();
        proposed.profiles.remove(position);
        if proposed.active_id.as_ref() == Some(id) {
            proposed.active_id = proposed
                .profiles
                .get(position)
                .or_else(|| proposed.profiles.last())
                .map(|entry| entry.id.clone());
        }
        if let Some(next_active) = &proposed.active_id
            && index.active_id.as_ref() == Some(id)
        {
            self.load_profile(next_active)?;
        }
        let index_commit = self.persist_index(&proposed)?;
        *index = proposed;
        let active = index.active_id.clone();
        if let IndexCommit::MaintenanceDebt(source) = index_commit {
            return Err(ProfileError::IndexMaintenanceDebt {
                id: id.clone(),
                source,
            });
        }
        match self.remove_profile_directory(id) {
            Ok(()) => Ok(active),
            Err(source) => Err(ProfileError::CleanupDebt {
                id: id.clone(),
                source,
            }),
        }
    }

    pub fn find_exact_source(&self, trimmed_url: &str) -> Result<Option<ProfileId>, ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        for entry in &index.profiles {
            match self.load_profile(&entry.id) {
                Ok((_, snapshot)) if snapshot.subscription_source_url() == Some(trimmed_url) => {
                    return Ok(Some(entry.id.clone()));
                }
                Ok(_) | Err(ProfileError::Unavailable(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn require_entry(&self, index: &ProfileIndex, id: &ProfileId) -> Result<usize, ProfileError> {
        index
            .profiles
            .iter()
            .position(|entry| &entry.id == id)
            .ok_or_else(|| ProfileError::NotFound(id.clone()))
    }

    fn load_profile(&self, id: &ProfileId) -> Result<(u64, Arc<Snapshot>), ProfileError> {
        #[cfg(test)]
        if self
            .load_fault
            .lock()
            .expect("profile load fault lock poisoned")
            .as_ref()
            == Some(id)
        {
            return Err(ProfileError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected profile load failure",
            )));
        }
        let profile = self.open_profile(id)?;
        let store = PersistentSnapshotStore::open_capability(profile)?;
        store
            .current_with_generation()
            .ok_or_else(|| ProfileError::Unavailable(id.clone()))
    }

    fn open_profile(&self, id: &ProfileId) -> Result<DirectoryCapability, ProfileError> {
        match self.root.open_child(OsStr::new(id.as_str())) {
            Ok(profile) => Ok(profile),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidData
                ) || is_link_loop_error(&error) =>
            {
                Err(ProfileError::Unavailable(id.clone()))
            }
            Err(error) => Err(ProfileError::Io(error)),
        }
    }

    fn remove_profile_directory(&self, id: &ProfileId) -> io::Result<()> {
        self.root.remove_child_tree(OsStr::new(id.as_str()))?;
        self.root.sync()
    }

    fn persist_index(&self, index: &ProfileIndex) -> Result<IndexCommit, ProfileError> {
        validate_index(index)?;
        let bytes = serde_json::to_vec(index)
            .map_err(|error| ProfileError::InvalidIndex(error.to_string()))?;
        let result = (|| {
            if self
                .root
                .file_metadata(OsStr::new(INDEX_TEMP_FILE))?
                .is_some()
            {
                self.root.remove_file(OsStr::new(INDEX_TEMP_FILE))?;
            }
            self.root.write_file(OsStr::new(INDEX_TEMP_FILE), &bytes)?;
            #[cfg(test)]
            if self.take_index_fault(1) {
                return Err(io::Error::other("injected pre-replace index failure"));
            }
            self.root
                .replace_file(OsStr::new(INDEX_TEMP_FILE), OsStr::new(INDEX_FILE))?;
            #[cfg(test)]
            if self.take_index_fault(2) {
                return Ok(IndexCommit::MaintenanceDebt(io::Error::other(
                    "injected post-replace index failure",
                )));
            }
            Ok(match self.root.sync() {
                Ok(()) => IndexCommit::Clean,
                Err(error) => IndexCommit::MaintenanceDebt(error),
            })
        })();
        match result {
            Ok(commit) => Ok(commit),
            Err(error) => {
                if let Err(cleanup) = self.root.remove_file(OsStr::new(INDEX_TEMP_FILE)) {
                    return Err(ProfileError::Io(io::Error::new(
                        cleanup.kind(),
                        format!(
                            "index update failed ({error}); temporary index cleanup failed: {cleanup}"
                        ),
                    )));
                }
                Err(ProfileError::Io(error))
            }
        }
    }

    #[cfg(test)]
    fn inject_index_fault(&self, point: u8) {
        self.index_fault
            .store(point, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn inject_load_fault(&self, id: ProfileId) {
        *self
            .load_fault
            .lock()
            .expect("profile load fault lock poisoned") = Some(id);
    }

    #[cfg(test)]
    fn take_index_fault(&self, point: u8) -> bool {
        self.index_fault
            .compare_exchange(
                point,
                0,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    #[cfg(test)]
    fn inject_directory_sync_fault(&self) {
        self.root.inject_sync_failure();
    }

    #[cfg(test)]
    fn directory_sync_call_count(&self) -> usize {
        self.root.sync_call_count()
    }
}

fn profile_directory_ids(root: &DirectoryCapability) -> Result<Vec<ProfileId>, ProfileError> {
    let mut ids = root
        .entries()?
        .into_iter()
        .filter(|entry| entry.is_directory)
        .filter_map(|entry| entry.name.into_string().ok())
        .filter_map(|name| ProfileId::parse(&name).ok())
        .collect::<Vec<_>>();
    ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    Ok(ids)
}

fn cleanup_unindexed_profile_directories(
    root: &DirectoryCapability,
    index: &ProfileIndex,
) -> Result<(), ProfileError> {
    let indexed = index
        .profiles
        .iter()
        .map(|entry| &entry.id)
        .collect::<HashSet<_>>();
    for id in profile_directory_ids(root)? {
        if !indexed.contains(&id)
            && let Err(source) = root
                .remove_child_tree(OsStr::new(id.as_str()))
                .and_then(|()| root.sync())
        {
            return Err(ProfileError::CleanupDebt { id, source });
        }
    }
    Ok(())
}

fn occupied_profile_slots(
    root: &DirectoryCapability,
    index: &ProfileIndex,
) -> Result<usize, ProfileError> {
    let mut occupied = index
        .profiles
        .iter()
        .map(|entry| entry.id.clone())
        .collect::<HashSet<_>>();
    occupied.extend(profile_directory_ids(root)?);
    Ok(occupied.len())
}

fn validate_index(index: &ProfileIndex) -> Result<(), ProfileError> {
    if index.schema_version != INDEX_SCHEMA_VERSION {
        return Err(ProfileError::InvalidIndex(
            "unsupported schema version".to_owned(),
        ));
    }
    if index.profiles.len() > MAX_PROFILES {
        return Err(ProfileError::InvalidIndex("too many profiles".to_owned()));
    }
    let mut ids = HashSet::with_capacity(index.profiles.len());
    if index.profiles.iter().any(|entry| !ids.insert(&entry.id)) {
        return Err(ProfileError::InvalidIndex(
            "duplicate profile ID".to_owned(),
        ));
    }
    if index.profiles.is_empty() != index.active_id.is_none() {
        return Err(ProfileError::InvalidIndex(
            "empty and active profile state disagree".to_owned(),
        ));
    }
    if index
        .active_id
        .as_ref()
        .is_some_and(|active| !ids.contains(active))
    {
        return Err(ProfileError::InvalidIndex(
            "active profile is not indexed".to_owned(),
        ));
    }
    Ok(())
}

fn is_valid_profile_id(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    value.bytes().enumerate().all(|(index, byte)| match index {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
    }) && value.as_bytes()[14] == b'4'
        && matches!(value.as_bytes()[19], b'8' | b'9' | b'a' | b'b')
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn map_root_open_error(error: io::Error) -> ProfileError {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::NotADirectory
    ) || is_link_loop_error(&error)
    {
        ProfileError::InvalidIndex("profile root is not a real directory".to_owned())
    } else {
        ProfileError::Io(error)
    }
}

#[cfg(unix)]
fn is_link_loop_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

#[cfg(not(unix))]
fn is_link_loop_error(_error: &io::Error) -> bool {
    false
}

fn map_lock_error(error: io::Error) -> ProfileError {
    let contention_code = fs2::lock_contended_error().raw_os_error();
    if contention_code.is_some() && error.raw_os_error() == contention_code {
        ProfileError::Locked
    } else {
        ProfileError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct TempDirectory(tempfile::TempDir);

    impl TempDirectory {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn path(&self) -> &Path {
            self.0.path()
        }
    }

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

    #[test]
    fn create_reverts_before_replace_and_reconciles_after_replace() {
        let pre_root = TempDirectory::new();
        let pre = ProfileStore::open(pre_root.path(), None).unwrap();
        pre.inject_index_fault(1);
        assert!(matches!(
            pre.create_profile(snapshot("https://pre.invalid/sub", "Pre")),
            Err(ProfileError::Io(_))
        ));
        assert!(pre.list().unwrap().is_empty());
        assert!(!pre_root.path().join(INDEX_FILE).exists());

        let post_root = TempDirectory::new();
        let post = ProfileStore::open(post_root.path(), None).unwrap();
        post.inject_index_fault(2);
        let id = match post.create_profile(snapshot("https://post.invalid/sub", "Post")) {
            Err(ProfileError::IndexMaintenanceDebt { id, .. }) => id,
            result => panic!("expected committed maintenance debt, got {result:?}"),
        };
        assert_eq!(post.active_id(), Some(id.clone()));
        drop(post);
        let reopened = ProfileStore::open(post_root.path(), None).unwrap();
        assert_eq!(reopened.active_id(), Some(id));
        assert_eq!(reopened.list().unwrap().len(), 1);
    }

    #[test]
    fn post_publication_directory_sync_failure_is_maintenance_debt() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let before = store.directory_sync_call_count();
        store.inject_directory_sync_fault();

        let id = match store.create_profile(snapshot("https://sync.invalid/sub", "Sync")) {
            Err(ProfileError::IndexMaintenanceDebt { id, source }) => {
                assert_eq!(source.kind(), io::ErrorKind::Other);
                id
            }
            result => panic!("expected post-publication maintenance debt, got {result:?}"),
        };
        assert_eq!(store.directory_sync_call_count(), before + 1);
        assert_eq!(store.active_id(), Some(id.clone()));
        drop(store);
        assert_eq!(
            ProfileStore::open(root.path(), None).unwrap().active_id(),
            Some(id)
        );
    }

    #[test]
    fn startup_removes_only_unindexed_uuid_directories() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let indexed = store
            .create_profile(snapshot("https://indexed.invalid/sub", "Indexed"))
            .unwrap();
        drop(store);

        let orphan = ProfileId::generate().unwrap();
        fs::create_dir(root.path().join(orphan.as_str())).unwrap();
        fs::create_dir(root.path().join("operator-notes")).unwrap();

        let reopened = ProfileStore::open(root.path(), None).unwrap();
        assert!(root.path().join(indexed.as_str()).is_dir());
        assert!(!root.path().join(orphan.as_str()).exists());
        assert!(root.path().join("operator-notes").is_dir());
        assert_eq!(reopened.list().unwrap().len(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn startup_surfaces_unresolved_orphan_cleanup() {
        let root = TempDirectory::new();
        let orphan = ProfileId::generate().unwrap();
        let orphan_path = root.path().join(orphan.as_str());
        fs::create_dir(&orphan_path).unwrap();
        let blocker = DirectoryCapability::open(&orphan_path).unwrap();

        assert!(matches!(
            ProfileStore::open(root.path(), None),
            Err(ProfileError::CleanupDebt { id, .. }) if id == orphan
        ));
        drop(blocker);
        assert!(ProfileStore::open(root.path(), None).is_ok());
    }

    #[test]
    fn unindexed_uuid_directories_consume_live_store_capacity() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        for index in 0..MAX_PROFILES - 1 {
            store
                .create_profile(snapshot(
                    &format!("https://{index}.invalid/sub"),
                    &format!("Node{index}"),
                ))
                .unwrap();
        }
        let orphan = ProfileId::generate().unwrap();
        fs::create_dir(root.path().join(orphan.as_str())).unwrap();

        assert!(matches!(
            store.create_profile(snapshot("https://overflow.invalid/sub", "Overflow")),
            Err(ProfileError::LimitReached)
        ));
    }

    #[test]
    fn activate_reverts_before_replace_and_reconciles_after_replace() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let first = store
            .create_profile(snapshot("https://one.invalid/sub", "One"))
            .unwrap();
        let second = store
            .create_profile(snapshot("https://two.invalid/sub", "Two"))
            .unwrap();
        assert_eq!(store.active_id(), Some(second.clone()));

        store.inject_index_fault(1);
        assert!(matches!(store.activate(&first), Err(ProfileError::Io(_))));
        assert_eq!(store.active_id(), Some(second));

        store.inject_index_fault(2);
        assert!(matches!(
            store.activate(&first),
            Err(ProfileError::IndexMaintenanceDebt { .. })
        ));
        assert_eq!(store.active_id(), Some(first.clone()));
        drop(store);
        assert_eq!(
            ProfileStore::open(root.path(), None).unwrap().active_id(),
            Some(first)
        );
    }

    #[test]
    fn delete_reverts_before_replace_and_never_resurrects_after_replace() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let first = store
            .create_profile(snapshot("https://one.invalid/sub", "One"))
            .unwrap();
        let second = store
            .create_profile(snapshot("https://two.invalid/sub", "Two"))
            .unwrap();

        store.inject_index_fault(1);
        assert!(matches!(store.delete(&first), Err(ProfileError::Io(_))));
        assert!(store.list().unwrap().iter().any(|item| item.id == first));
        assert!(root.path().join(first.as_str()).is_dir());

        store.inject_index_fault(2);
        assert!(matches!(
            store.delete(&first),
            Err(ProfileError::IndexMaintenanceDebt { .. })
        ));
        assert!(!store.list().unwrap().iter().any(|item| item.id == first));
        assert!(
            root.path().join(first.as_str()).is_dir(),
            "post-replace sync debt must retain rollback material"
        );
        drop(store);

        let reopened = ProfileStore::open(root.path(), None).unwrap();
        assert_eq!(reopened.active_id(), Some(second));
        assert!(!reopened.list().unwrap().iter().any(|item| item.id == first));
    }

    #[test]
    fn exact_source_lookup_propagates_operational_load_failures() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let id = store
            .create_profile(snapshot("https://one.invalid/sub", "One"))
            .unwrap();
        store.inject_load_fault(id);
        assert!(matches!(
            store.find_exact_source("https://missing.invalid/sub"),
            Err(ProfileError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn lock_mapping_only_classifies_the_platform_contention_code() {
        let contention = fs2::lock_contended_error()
            .raw_os_error()
            .expect("fs2 contention errors have an OS code");
        assert!(matches!(
            map_lock_error(io::Error::from_raw_os_error(contention)),
            ProfileError::Locked
        ));
        assert!(matches!(
            map_lock_error(io::Error::other("not contention")),
            ProfileError::Io(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn directory_capability_remains_on_original_after_path_replacement() {
        let root = TempDirectory::new();
        let guarded = root.path().join("guarded");
        let original = root.path().join("original");
        fs::create_dir(&guarded).unwrap();
        fs::write(guarded.join("identity"), b"original").unwrap();
        let capability = DirectoryCapability::open(&guarded).unwrap();

        fs::rename(&guarded, &original).unwrap();
        fs::create_dir(&guarded).unwrap();
        fs::write(guarded.join("identity"), b"replacement").unwrap();
        assert_eq!(
            capability.read_file(OsStr::new("identity"), 32).unwrap(),
            b"original"
        );

        fs::remove_dir(&guarded).unwrap();
        fs::rename(&original, &guarded).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn directory_capability_prevents_path_replacement() {
        let root = TempDirectory::new();
        let guarded = root.path().join("guarded");
        let replacement = root.path().join("replacement");
        fs::create_dir(&guarded).unwrap();
        let capability = DirectoryCapability::open(&guarded).unwrap();

        assert!(fs::rename(&guarded, &replacement).is_err());
        capability
            .write_file(OsStr::new("identity"), b"original")
            .unwrap();
        assert_eq!(fs::read(guarded.join("identity")).unwrap(), b"original");
    }
}
