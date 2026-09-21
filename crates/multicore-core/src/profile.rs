use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    fmt, fs, io,
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    PersistenceError, PersistentSnapshotStore, Snapshot,
    store_root::{DirectoryCapability, StoreLock},
};

pub const MAX_PROFILES: usize = 16;
pub const MAX_PROFILE_INDEX_BYTES: u64 = 16 * 1024;
pub const MAX_SELECTIONS_BYTES: u64 = 128 * 1024;
pub const MAX_SELECTIONS: usize = 64;
const INDEX_SCHEMA_VERSION: u32 = 1;
const SELECTIONS_SCHEMA_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const INDEX_TEMP_FILE: &str = "index.json.tmp";
const SELECTIONS_FILE: &str = "selections.json";
const SELECTIONS_TEMP_FILE: &str = "selections.json.tmp";
const REVISION_DOMAIN: &[u8] = b"multicore.catalog-revision.v1\0";

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

#[derive(Clone)]
pub struct LoadedProfile {
    pub summary: ProfileSummary,
    pub revision: Option<u64>,
    pub physical_generation: Option<u64>,
    pub snapshot: Option<Arc<Snapshot>>,
}

pub type ActiveSnapshotGeneration = (ProfileId, u64, u64, Arc<Snapshot>);

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
    #[error("profile files require cleanup")]
    CleanupDebt {
        id: ProfileId,
        mutation_committed: bool,
        #[source]
        source: io::Error,
    },
    #[error("profile mutation committed but durability maintenance is required")]
    IndexMaintenanceDebt {
        id: ProfileId,
        #[source]
        source: io::Error,
    },
    #[error("profile snapshot became unreadable after the mutation committed")]
    PostCommitRead {
        id: ProfileId,
        #[source]
        source: Box<ProfileError>,
    },
    #[error("profile selection mutation committed but durability maintenance is required")]
    SelectionMaintenanceDebt {
        id: ProfileId,
        #[source]
        source: io::Error,
    },
    #[error("profile selection is invalid: {0}")]
    InvalidSelection(&'static str),
}

impl ProfileError {
    /// Reports whether the requested mutation was published before the error occurred.
    pub fn mutation_committed(&self) -> bool {
        match self {
            Self::IndexMaintenanceDebt { .. }
            | Self::SelectionMaintenanceDebt { .. }
            | Self::PostCommitRead { .. } => true,
            Self::CleanupDebt {
                mutation_committed, ..
            } => *mutation_committed,
            _ => false,
        }
    }
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSelections {
    schema_version: u32,
    catalog_revision: u64,
    selections: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct ProfileStore {
    root: DirectoryCapability,
    _lock: StoreLock,
    index: RwLock<ProfileIndex>,
    selection_lock: Mutex<()>,
    maintenance_debt: AtomicBool,
    #[cfg(test)]
    index_fault: std::sync::atomic::AtomicU8,
    #[cfg(test)]
    selection_fault: std::sync::atomic::AtomicU8,
    #[cfg(test)]
    load_fault: std::sync::Mutex<Option<ProfileId>>,
    #[cfg(test)]
    load_calls: std::sync::atomic::AtomicUsize,
}

enum IndexCommit {
    Clean,
    MaintenanceDebt(io::Error),
}

impl ProfileStore {
    pub fn open(
        root: impl Into<PathBuf>,
        legacy_root: Option<PathBuf>,
    ) -> Result<Self, ProfileError> {
        Self::open_impl(root.into(), legacy_root, false)
    }

    #[cfg(test)]
    fn open_with_migration_snapshot_sync_fault(
        root: impl Into<PathBuf>,
        legacy_root: Option<PathBuf>,
    ) -> Result<Self, ProfileError> {
        Self::open_impl(root.into(), legacy_root, true)
    }

    fn open_impl(
        root: PathBuf,
        legacy_root: Option<PathBuf>,
        migration_snapshot_sync_fault: bool,
    ) -> Result<Self, ProfileError> {
        fs::create_dir_all(&root)?;
        let root = DirectoryCapability::open(&root).map_err(map_root_open_error)?;
        let lock = root.lock_exclusive().map_err(map_lock_error)?;
        let index_metadata = root.file_metadata(OsStr::new(INDEX_FILE))?;
        let index = match index_metadata {
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
        // Establish the validated index (or its absence) durably before deleting any
        // unindexed rollback material. A crash must not resurrect an old index that names
        // a directory cleanup has already removed.
        root.sync()?;
        let store = Self {
            root,
            _lock: lock,
            index: RwLock::new(index),
            selection_lock: Mutex::new(()),
            maintenance_debt: AtomicBool::new(false),
            #[cfg(test)]
            index_fault: std::sync::atomic::AtomicU8::new(0),
            #[cfg(test)]
            selection_fault: std::sync::atomic::AtomicU8::new(0),
            #[cfg(test)]
            load_fault: std::sync::Mutex::new(None),
            #[cfg(test)]
            load_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        if index_metadata.is_none()
            && let Some(legacy_root) = legacy_root
        {
            store.migrate_legacy(&legacy_root, migration_snapshot_sync_fault)?;
        }
        let index = store.index.read().expect("profile index lock poisoned");
        cleanup_unindexed_profile_directories(&store.root, &index)?;
        drop(index);
        Ok(store)
    }

    pub fn list(&self) -> Result<Vec<ProfileSummary>, ProfileError> {
        self.map_loaded(|profile| profile.summary)
    }

    /// Enumerates the ordered index while loading each profile at most once.
    ///
    /// Unreadable profiles remain present with `available = false` and no snapshot metadata.
    pub fn list_loaded(&self) -> Result<Vec<LoadedProfile>, ProfileError> {
        self.map_loaded(std::convert::identity)
    }

    /// Projects each ordered profile while its single loaded snapshot is alive, bounding callers
    /// that only retain presentation metadata to one full snapshot at a time.
    pub fn map_loaded<T>(
        &self,
        mut project: impl FnMut(LoadedProfile) -> T,
    ) -> Result<Vec<T>, ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        Ok(index
            .profiles
            .iter()
            .map(|entry| {
                let loaded = self.load_profile(&entry.id).ok();
                project(LoadedProfile {
                    summary: ProfileSummary {
                        id: entry.id.clone(),
                        created_at_unix: entry.created_at_unix,
                        active: index.active_id.as_ref() == Some(&entry.id),
                        available: loaded.is_some(),
                    },
                    revision: loaded
                        .as_ref()
                        .map(|(generation, _)| catalog_revision(&entry.id, *generation)),
                    physical_generation: loaded.as_ref().map(|(generation, _)| *generation),
                    snapshot: loaded.map(|(_, snapshot)| snapshot),
                })
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

    pub fn contains(&self, id: &ProfileId) -> bool {
        self.index
            .read()
            .expect("profile index lock poisoned")
            .profiles
            .iter()
            .any(|entry| &entry.id == id)
    }

    /// Retries cleanup only after an ID is no longer authoritative in the profile index.
    pub fn cleanup_unindexed_profile(&self, id: &ProfileId) -> Result<bool, ProfileError> {
        if self.contains(id) {
            return Ok(false);
        }
        match self.remove_profile_directory(id) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(ProfileError::Io(error)),
        }
    }

    pub fn active_snapshot(&self) -> Result<Option<(ProfileId, u64, Arc<Snapshot>)>, ProfileError> {
        Ok(self
            .active_snapshot_generation()?
            .map(|(id, revision, _, snapshot)| (id, revision, snapshot)))
    }

    pub fn active_snapshot_generation(
        &self,
    ) -> Result<Option<ActiveSnapshotGeneration>, ProfileError> {
        let Some(id) = self.active_id() else {
            return Ok(None);
        };
        let (generation, snapshot) = self.load_profile(&id)?;
        Ok(Some((
            id.clone(),
            catalog_revision(&id, generation),
            generation,
            snapshot,
        )))
    }

    /// Loads an indexed profile without changing which profile is active.
    pub fn profile_snapshot(&self, id: &ProfileId) -> Result<(u64, Arc<Snapshot>), ProfileError> {
        self.profile_snapshot_generation(id)
            .map(|(revision, _, snapshot)| (revision, snapshot))
    }

    pub fn profile_snapshot_generation(
        &self,
        id: &ProfileId,
    ) -> Result<(u64, u64, Arc<Snapshot>), ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let (generation, snapshot) = self.load_profile(id)?;
        Ok((catalog_revision(id, generation), generation, snapshot))
    }

    pub fn has_maintenance_debt(&self) -> bool {
        self.maintenance_debt.load(Ordering::Acquire)
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
            let (_, maintenance_debt) = store.commit_status((*snapshot).clone())?.into_parts();
            drop(store);
            self.load_profile(&id)?;
            Ok::<_, ProfileError>(maintenance_debt)
        })();
        let snapshot_debt = match result {
            Ok(debt) => debt,
            Err(error) => {
                if let Err(source) = self.remove_profile_directory(&id) {
                    self.maintenance_debt.store(true, Ordering::Release);
                    return Err(ProfileError::CleanupDebt {
                        id,
                        mutation_committed: false,
                        source,
                    });
                }
                return Err(error);
            }
        };

        let mut proposed = index.clone();
        proposed.profiles.push(ProfileIndexEntry {
            id: id.clone(),
            created_at_unix: unix_timestamp(),
        });
        proposed.active_id = Some(id.clone());
        match self.persist_index(&proposed) {
            Ok(IndexCommit::Clean) => {
                *index = proposed;
                match snapshot_debt {
                    Some(source) => {
                        self.maintenance_debt.store(true, Ordering::Release);
                        Err(ProfileError::IndexMaintenanceDebt { id, source })
                    }
                    None => Ok(id),
                }
            }
            Ok(IndexCommit::MaintenanceDebt(source)) => {
                *index = proposed;
                self.maintenance_debt.store(true, Ordering::Release);
                Err(ProfileError::IndexMaintenanceDebt {
                    id,
                    source: combine_maintenance_debt(snapshot_debt, source),
                })
            }
            Err(error) => match self.remove_profile_directory(&id) {
                Ok(()) => Err(error),
                Err(source) => {
                    self.maintenance_debt.store(true, Ordering::Release);
                    Err(ProfileError::CleanupDebt {
                        id,
                        mutation_committed: false,
                        source,
                    })
                }
            },
        }
    }

    pub fn replace_profile(
        &self,
        id: &ProfileId,
        snapshot: Arc<Snapshot>,
    ) -> Result<u64, ProfileError> {
        self.replace_profile_loaded(id, snapshot)
            .map(|(revision, _, _)| revision)
    }

    pub fn replace_profile_loaded(
        &self,
        id: &ProfileId,
        snapshot: Arc<Snapshot>,
    ) -> Result<(u64, u64, Arc<Snapshot>), ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let profile = self.open_profile(id)?;
        let store = PersistentSnapshotStore::open_capability(profile)?;
        let ((generation, _), maintenance_debt) = store
            .commit_with_generation_status((*snapshot).clone())?
            .into_parts();
        drop(store);
        let (reloaded_generation, reloaded) =
            self.load_profile(id)
                .map_err(|source| ProfileError::PostCommitRead {
                    id: id.clone(),
                    source: Box::new(source),
                })?;
        if reloaded_generation != generation {
            return Err(ProfileError::PostCommitRead {
                id: id.clone(),
                source: Box::new(ProfileError::Unavailable(id.clone())),
            });
        }
        match maintenance_debt {
            Some(source) => {
                self.maintenance_debt.store(true, Ordering::Release);
                Err(ProfileError::IndexMaintenanceDebt {
                    id: id.clone(),
                    source,
                })
            }
            None => Ok((catalog_revision(id, generation), generation, reloaded)),
        }
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
                    self.maintenance_debt.store(true, Ordering::Release);
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
            self.maintenance_debt.store(true, Ordering::Release);
            return Err(ProfileError::IndexMaintenanceDebt {
                id: id.clone(),
                source,
            });
        }
        match self.remove_profile_directory(id) {
            Ok(()) => Ok(active),
            Err(source) => {
                self.maintenance_debt.store(true, Ordering::Release);
                Err(ProfileError::CleanupDebt {
                    id: id.clone(),
                    mutation_committed: true,
                    source,
                })
            }
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

    pub fn save_selection(
        &self,
        id: &ProfileId,
        revision: u64,
        group_id: &str,
        node_id: &str,
    ) -> Result<(), ProfileError> {
        validate_selection_id(group_id)?;
        validate_selection_id(node_id)?;
        if revision == 0 {
            return Err(ProfileError::InvalidSelection("catalog revision is zero"));
        }

        let _guard = self
            .selection_lock
            .lock()
            .expect("profile selection lock poisoned");
        let index = self.index.read().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let profile = self.open_profile(id)?;
        let mut persisted = load_selections(&profile)?
            .filter(|saved| {
                saved.schema_version == SELECTIONS_SCHEMA_VERSION
                    && saved.catalog_revision == revision
                    && selections_are_valid(&saved.selections)
            })
            .unwrap_or_else(|| PersistedSelections {
                schema_version: SELECTIONS_SCHEMA_VERSION,
                catalog_revision: revision,
                selections: BTreeMap::new(),
            });
        if !persisted.selections.contains_key(group_id)
            && persisted.selections.len() >= MAX_SELECTIONS
        {
            return Err(ProfileError::InvalidSelection("selection limit reached"));
        }
        persisted
            .selections
            .insert(group_id.to_owned(), node_id.to_owned());
        let bytes = serde_json::to_vec(&persisted)
            .map_err(|_| ProfileError::InvalidSelection("selection serialization failed"))?;
        if bytes.len() as u64 > MAX_SELECTIONS_BYTES {
            return Err(ProfileError::InvalidSelection(
                "selections exceed size limit",
            ));
        }
        #[cfg(test)]
        let selection_fault = self.selection_fault.swap(0, Ordering::SeqCst);
        #[cfg(not(test))]
        let selection_fault = 0;
        match persist_selections(&profile, &bytes, selection_fault) {
            Ok(IndexCommit::Clean) => Ok(()),
            Ok(IndexCommit::MaintenanceDebt(source)) => {
                self.maintenance_debt.store(true, Ordering::Release);
                Err(ProfileError::SelectionMaintenanceDebt {
                    id: id.clone(),
                    source,
                })
            }
            Err(error) => Err(ProfileError::Io(error)),
        }
    }

    pub fn valid_selections(
        &self,
        id: &ProfileId,
        revision: u64,
        catalog: &[(&str, &[&str])],
    ) -> Result<BTreeMap<String, String>, ProfileError> {
        let _guard = self
            .selection_lock
            .lock()
            .expect("profile selection lock poisoned");
        let index = self.index.read().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let profile = self.open_profile(id)?;
        let Some(saved) = load_selections(&profile)? else {
            return Ok(BTreeMap::new());
        };
        if saved.schema_version != SELECTIONS_SCHEMA_VERSION
            || saved.catalog_revision != revision
            || !selections_are_valid(&saved.selections)
        {
            return Ok(BTreeMap::new());
        }
        Ok(saved
            .selections
            .into_iter()
            .filter(|(group, node)| {
                catalog.iter().any(|(catalog_group, catalog_nodes)| {
                    group == catalog_group
                        && catalog_nodes
                            .iter()
                            .any(|catalog_node| node == catalog_node)
                })
            })
            .collect())
    }

    fn migrate_legacy(
        &self,
        legacy_path: &std::path::Path,
        snapshot_sync_fault: bool,
    ) -> Result<(), ProfileError> {
        let snapshot = match PersistentSnapshotStore::load_latest_readonly(legacy_path) {
            Ok(snapshot) => snapshot,
            Err(PersistenceError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(ProfileError::Snapshot(error)),
        };
        let Some(snapshot) = snapshot else {
            return Ok(());
        };

        let id = loop {
            let candidate = ProfileId::generate()?;
            if self
                .root
                .file_metadata(OsStr::new(candidate.as_str()))?
                .is_none()
            {
                break candidate;
            }
        };
        let profile = self.root.create_child(OsStr::new(id.as_str()))?;
        let migration = (|| {
            let destination = PersistentSnapshotStore::open_capability(profile)?;
            #[cfg(test)]
            if snapshot_sync_fault {
                destination.inject_directory_sync_failure();
            }
            #[cfg(not(test))]
            let _ = snapshot_sync_fault;
            let (_, snapshot_debt) = destination.commit_status((*snapshot).clone())?.into_parts();
            if let Some(source) = snapshot_debt {
                return Err(ProfileError::Io(source));
            }
            drop(destination);
            self.load_profile(&id)?;
            let proposed = ProfileIndex {
                schema_version: INDEX_SCHEMA_VERSION,
                profiles: vec![ProfileIndexEntry {
                    id: id.clone(),
                    created_at_unix: unix_timestamp(),
                }],
                active_id: Some(id.clone()),
            };
            let index_commit = self.persist_index(&proposed)?;
            *self.index.write().expect("profile index lock poisoned") = proposed;
            Ok::<_, ProfileError>(index_commit)
        })();
        match migration {
            Ok(index_commit) => {
                if matches!(index_commit, IndexCommit::MaintenanceDebt(_)) {
                    self.maintenance_debt.store(true, Ordering::Release);
                }
                Ok(())
            }
            Err(error) => match self.remove_profile_directory(&id) {
                Ok(()) => Err(error),
                Err(source) => {
                    self.maintenance_debt.store(true, Ordering::Release);
                    Err(ProfileError::CleanupDebt {
                        id,
                        mutation_committed: false,
                        source,
                    })
                }
            },
        }
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
        self.load_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    fn inject_selection_fault(&self, point: u8) {
        self.selection_fault.store(point, Ordering::SeqCst);
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

fn profile_entry_ids(root: &DirectoryCapability) -> Result<Vec<ProfileId>, ProfileError> {
    let mut ids = root
        .entries()?
        .into_iter()
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
    let mut removed_any = false;
    for id in profile_entry_ids(root)? {
        if !indexed.contains(&id) {
            if let Err(source) = root.remove_entry_nofollow(OsStr::new(id.as_str())) {
                if removed_any && let Err(sync) = root.sync() {
                    return Err(ProfileError::CleanupDebt {
                        id,
                        mutation_committed: false,
                        source: io::Error::new(
                            source.kind(),
                            format!(
                                "orphan cleanup failed ({source}); prior cleanup sync failed: {sync}"
                            ),
                        ),
                    });
                }
                return Err(ProfileError::CleanupDebt {
                    id,
                    mutation_committed: false,
                    source,
                });
            }
            removed_any = true;
        }
    }
    if removed_any {
        root.sync()?;
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
    occupied.extend(profile_entry_ids(root)?);
    Ok(occupied.len())
}

fn catalog_revision(id: &ProfileId, physical_generation: u64) -> u64 {
    let mut digest = Sha256::new();
    digest.update(REVISION_DOMAIN);
    digest.update(id.as_str().as_bytes());
    digest.update(physical_generation.to_be_bytes());
    let bytes: [u8; 8] = digest.finalize()[..8]
        .try_into()
        .expect("SHA-256 prefix has fixed length");
    u64::from_be_bytes(bytes) | (1_u64 << 63)
}

fn validate_selection_id(value: &str) -> Result<(), ProfileError> {
    if value.is_empty() || value.len() > 256 {
        return Err(ProfileError::InvalidSelection(
            "identifier length is out of bounds",
        ));
    }
    if !value
        .bytes()
        .all(|byte| (0x21..=0x7e).contains(&byte) && !matches!(byte, b'/' | b'\\'))
    {
        return Err(ProfileError::InvalidSelection(
            "identifier contains an unsafe byte",
        ));
    }
    Ok(())
}

fn selections_are_valid(selections: &BTreeMap<String, String>) -> bool {
    selections.len() <= MAX_SELECTIONS
        && selections.iter().all(|(group, node)| {
            validate_selection_id(group).is_ok() && validate_selection_id(node).is_ok()
        })
}

fn load_selections(profile: &DirectoryCapability) -> io::Result<Option<PersistedSelections>> {
    let Some((regular, len)) = profile.file_metadata(OsStr::new(SELECTIONS_FILE))? else {
        return Ok(None);
    };
    if !regular || len > MAX_SELECTIONS_BYTES {
        return Ok(None);
    }
    let bytes = match profile.read_file(OsStr::new(SELECTIONS_FILE), MAX_SELECTIONS_BYTES) {
        Ok(bytes) => bytes,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

fn persist_selections(
    profile: &DirectoryCapability,
    bytes: &[u8],
    fault_point: u8,
) -> io::Result<IndexCommit> {
    let result = (|| {
        if profile
            .file_metadata(OsStr::new(SELECTIONS_TEMP_FILE))?
            .is_some()
        {
            profile.remove_entry_nofollow(OsStr::new(SELECTIONS_TEMP_FILE))?;
        }
        profile.write_file(OsStr::new(SELECTIONS_TEMP_FILE), bytes)?;
        if fault_point == 1 {
            return Err(io::Error::other("injected pre-replace selection failure"));
        }
        profile.replace_file(
            OsStr::new(SELECTIONS_TEMP_FILE),
            OsStr::new(SELECTIONS_FILE),
        )?;
        if fault_point == 2 {
            return Ok(IndexCommit::MaintenanceDebt(io::Error::other(
                "injected post-replace selection failure",
            )));
        }
        Ok(match profile.sync() {
            Ok(()) => IndexCommit::Clean,
            Err(error) => IndexCommit::MaintenanceDebt(error),
        })
    })();
    match result {
        Ok(commit) => Ok(commit),
        Err(error) => {
            if let Err(cleanup) = profile.remove_entry_nofollow(OsStr::new(SELECTIONS_TEMP_FILE)) {
                return Err(io::Error::new(
                    cleanup.kind(),
                    format!(
                        "selection update failed ({error}); temporary selection cleanup failed: {cleanup}"
                    ),
                ));
            }
            Err(error)
        }
    }
}

fn combine_maintenance_debt(snapshot: Option<io::Error>, index: io::Error) -> io::Error {
    match snapshot {
        Some(snapshot) => io::Error::new(
            index.kind(),
            format!(
                "snapshot directory sync failed ({snapshot}); index directory sync failed ({index})"
            ),
        ),
        None => index,
    }
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
    fn selection_faults_distinguish_uncommitted_and_committed_outcomes() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let id = store
            .create_profile(snapshot("https://selection.invalid/sub", "Selection"))
            .unwrap();
        let revision = store.profile_snapshot(&id).unwrap().0;

        store.inject_selection_fault(1);
        let pre = store
            .save_selection(&id, revision, "group-pre", "node-pre")
            .unwrap_err();
        assert!(!pre.mutation_committed());
        assert!(
            store
                .valid_selections(&id, revision, &[("group-pre", &["node-pre"])])
                .unwrap()
                .is_empty()
        );

        store.inject_selection_fault(2);
        let post = store
            .save_selection(&id, revision, "group-post", "node-post")
            .unwrap_err();
        assert!(matches!(
            &post,
            ProfileError::SelectionMaintenanceDebt { .. }
        ));
        assert!(post.mutation_committed());
        assert!(store.has_maintenance_debt());
        assert_eq!(
            store
                .valid_selections(&id, revision, &[("group-post", &["node-post"])])
                .unwrap()["group-post"],
            "node-post"
        );
    }

    #[test]
    fn migration_snapshot_sync_debt_never_publishes_the_index() {
        let root = TempDirectory::new();
        let legacy_path = root.path().join("legacy");
        PersistentSnapshotStore::open(&legacy_path)
            .unwrap()
            .commit((*snapshot("https://legacy.invalid/sub", "Legacy")).clone())
            .unwrap();
        let profiles = root.path().join("profiles");

        let error = ProfileStore::open_with_migration_snapshot_sync_fault(
            &profiles,
            Some(legacy_path.clone()),
        )
        .unwrap_err();
        assert!(!error.mutation_committed());
        assert!(!profiles.join(INDEX_FILE).exists());
        assert!(
            profile_entry_ids(&DirectoryCapability::open(&profiles).unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(legacy_path.join("snapshot-00000000000000000001").is_dir());

        let retried = ProfileStore::open(&profiles, Some(legacy_path)).unwrap();
        assert_eq!(retried.list().unwrap().len(), 1);
    }

    #[test]
    fn startup_removes_only_unindexed_uuid_entries() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let indexed = store
            .create_profile(snapshot("https://indexed.invalid/sub", "Indexed"))
            .unwrap();
        drop(store);

        let orphan = ProfileId::generate().unwrap();
        let orphan_file = ProfileId::generate().unwrap();
        fs::create_dir(root.path().join(orphan.as_str())).unwrap();
        fs::write(root.path().join(orphan_file.as_str()), b"rollback debris").unwrap();
        fs::create_dir(root.path().join("operator-notes")).unwrap();

        let reopened = ProfileStore::open(root.path(), None).unwrap();
        assert!(root.path().join(indexed.as_str()).is_dir());
        assert!(!root.path().join(orphan.as_str()).exists());
        assert!(!root.path().join(orphan_file.as_str()).exists());
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
    fn every_unindexed_uuid_entry_consumes_live_store_capacity() {
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
        fs::write(
            root.path().join(orphan.as_str()),
            b"not a profile directory",
        )
        .unwrap();

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
    fn loaded_enumeration_parses_each_profile_once() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        store
            .create_profile(snapshot("https://one.invalid/sub", "One"))
            .unwrap();
        store
            .create_profile(snapshot("https://two.invalid/sub", "Two"))
            .unwrap();
        store.load_calls.store(0, Ordering::SeqCst);

        let loaded = store.list_loaded().unwrap();

        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|profile| profile.snapshot.is_some()));
        assert_eq!(store.load_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn replace_load_failure_is_reported_as_committed() {
        let root = TempDirectory::new();
        let store = ProfileStore::open(root.path(), None).unwrap();
        let id = store
            .create_profile(snapshot("https://one.invalid/sub", "One"))
            .unwrap();
        store.inject_load_fault(id.clone());

        let error = store
            .replace_profile(&id, snapshot("https://one.invalid/sub", "Updated"))
            .unwrap_err();

        assert!(matches!(&error, ProfileError::PostCommitRead { .. }));
        assert!(error.mutation_committed());
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

        fs::remove_dir_all(&guarded).unwrap();
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
