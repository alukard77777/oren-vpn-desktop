use std::{
    collections::HashSet,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    PersistenceError, PersistentSnapshotStore, Snapshot,
    snapshot::{secure_directory, secure_file, sync_directory},
};

pub const MAX_PROFILES: usize = 16;
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
    root: PathBuf,
    index: RwLock<ProfileIndex>,
}

impl ProfileStore {
    pub fn open(
        root: impl Into<PathBuf>,
        _legacy_root: Option<PathBuf>,
    ) -> Result<Self, ProfileError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        let index_path = root.join(INDEX_FILE);
        let index = match fs::symlink_metadata(&index_path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(ProfileError::InvalidIndex(
                        "index is not a regular file".to_owned(),
                    ));
                }
                secure_file(&index_path)?;
                let bytes = fs::read(&index_path)?;
                let index: ProfileIndex = serde_json::from_slice(&bytes)
                    .map_err(|error| ProfileError::InvalidIndex(error.to_string()))?;
                validate_index(&index)?;
                index
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => ProfileIndex {
                schema_version: INDEX_SCHEMA_VERSION,
                profiles: Vec::new(),
                active_id: None,
            },
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            root,
            index: RwLock::new(index),
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
        if index.profiles.len() >= MAX_PROFILES {
            return Err(ProfileError::LimitReached);
        }
        let id = loop {
            let candidate = ProfileId::generate()?;
            if !index.profiles.iter().any(|entry| entry.id == candidate)
                && !self.profile_path(&candidate).exists()
            {
                break candidate;
            }
        };
        let profile_path = self.profile_path(&id);
        let result = (|| {
            let store = PersistentSnapshotStore::open(&profile_path)?;
            store.commit((*snapshot).clone())?;
            drop(store);
            self.load_profile(&id)?;
            Ok::<_, ProfileError>(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&profile_path);
            return Err(error);
        }

        let mut proposed = index.clone();
        proposed.profiles.push(ProfileIndexEntry {
            id: id.clone(),
            created_at_unix: unix_timestamp(),
        });
        proposed.active_id = Some(id.clone());
        if let Err(error) = self.persist_index(&proposed) {
            let _ = fs::remove_dir_all(&profile_path);
            return Err(error);
        }
        *index = proposed;
        Ok(id)
    }

    pub fn replace_profile(
        &self,
        id: &ProfileId,
        snapshot: Arc<Snapshot>,
    ) -> Result<u64, ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        self.require_entry(&index, id)?;
        let store = PersistentSnapshotStore::open(self.profile_path(id))?;
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
            self.persist_index(&proposed)?;
            *index = proposed;
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
        self.persist_index(&proposed)?;
        *index = proposed;
        let active = index.active_id.clone();
        let profile_path = self.profile_path(id);
        match fs::remove_dir_all(&profile_path) {
            Ok(()) => Ok(active),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(active),
            Err(source) => Err(ProfileError::CleanupDebt {
                id: id.clone(),
                source,
            }),
        }
    }

    pub fn find_exact_source(&self, trimmed_url: &str) -> Result<Option<ProfileId>, ProfileError> {
        let index = self.index.read().expect("profile index lock poisoned");
        for entry in &index.profiles {
            if let Ok((_, snapshot)) = self.load_profile(&entry.id)
                && snapshot.subscription_source_url() == Some(trimmed_url)
            {
                return Ok(Some(entry.id.clone()));
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

    fn profile_path(&self, id: &ProfileId) -> PathBuf {
        debug_assert!(is_valid_profile_id(id.as_str()));
        self.root.join(id.as_str())
    }

    fn load_profile(&self, id: &ProfileId) -> Result<(u64, Arc<Snapshot>), ProfileError> {
        let path = self.profile_path(id);
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| ProfileError::Unavailable(id.clone()))?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(ProfileError::Unavailable(id.clone()));
        }
        let store = PersistentSnapshotStore::open(path)?;
        store
            .current_with_generation()
            .ok_or_else(|| ProfileError::Unavailable(id.clone()))
    }

    fn persist_index(&self, index: &ProfileIndex) -> Result<(), ProfileError> {
        validate_index(index)?;
        let bytes = serde_json::to_vec(index)
            .map_err(|error| ProfileError::InvalidIndex(error.to_string()))?;
        let temporary = self.root.join(INDEX_TEMP_FILE);
        let target = self.root.join(INDEX_FILE);
        let result = (|| {
            match fs::symlink_metadata(&temporary) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "profile index temporary path is a directory",
                    ));
                }
                Ok(_) => fs::remove_file(&temporary)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            secure_file(&temporary)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            atomic_replace(&temporary, &target)?;
            secure_file(&target)?;
            sync_directory(&self.root)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(ProfileError::Io)
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

#[cfg(unix)]
fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn atomic_replace(_source: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic index replacement unsupported",
    ))
}
