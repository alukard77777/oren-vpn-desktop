use multicore_core::RuntimePaths;
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::OnceLock,
};

const HOST_NAME: &str = "multicore-core-host";
const STICKY: u32 = 0o1000;
const GROUP_OR_OTHER_WRITE: u32 = 0o022;

static CONTEXT: OnceLock<UserContext> = OnceLock::new();

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ResolveError {
    InvalidGeneration,
    Context,
    Layout,
    Symlink,
    Escape,
    Missing,
    Ownership,
    InvalidElf,
    Io,
}

#[derive(Debug)]
pub(crate) struct UserContext {
    pub(crate) uid: u32,
    pub(crate) data_root: PathBuf,
}

pub(crate) struct ResolvedRuntime {
    pub paths: RuntimePaths,
    guards: Vec<GuardedPath>,
}

struct GuardedPath {
    path: PathBuf,
    handle: File,
    identity: (u64, u64),
}

impl ResolvedRuntime {
    pub(crate) fn verify_unchanged(&self) -> Result<(), ResolveError> {
        for guard in &self.guards {
            let metadata = fs::symlink_metadata(&guard.path).map_err(|_| ResolveError::Missing)?;
            if metadata.file_type().is_symlink() {
                return Err(ResolveError::Symlink);
            }
            if (metadata.dev(), metadata.ino()) != guard.identity {
                return Err(ResolveError::Escape);
            }
        }
        Ok(())
    }
}

pub(crate) fn install_context(context: UserContext) -> Result<(), ResolveError> {
    CONTEXT.set(context).map_err(|_| ResolveError::Context)
}

pub(crate) fn resolve_current(generation: u64) -> Result<ResolvedRuntime, ResolveError> {
    let context = CONTEXT.get().ok_or(ResolveError::Context)?;
    let executable = std::env::current_exe().map_err(|_| ResolveError::Io)?;
    resolve_from(&context.data_root, &executable, generation, context.uid)
}

pub(crate) fn user_data_root(
    uid: u32,
    passwd: &str,
    peer_environment: &[u8],
) -> Result<PathBuf, ResolveError> {
    let home = passwd
        .lines()
        .find_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            (fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid))
                .then(|| fields[5].to_owned())
        })
        .ok_or(ResolveError::Context)?;
    let xdg_data_home = peer_environment
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"XDG_DATA_HOME="))
        .and_then(|value| std::str::from_utf8(value).ok());
    multicore_platform::dirs::unix_data_root(xdg_data_home.map(OsStr::new), Some(OsStr::new(&home)))
        .map_err(|_| ResolveError::Context)
}

pub(crate) fn resolve_from(
    data_root: &Path,
    host_executable: &Path,
    generation: u64,
    owner_uid: u32,
) -> Result<ResolvedRuntime, ResolveError> {
    if generation == 0 {
        return Err(ResolveError::InvalidGeneration);
    }
    let runtime_dir = host_executable.parent().ok_or(ResolveError::Layout)?;
    if runtime_dir.file_name().and_then(|value| value.to_str()) != Some("runtime")
        || host_executable.file_name().and_then(|value| value.to_str()) != Some(HOST_NAME)
    {
        return Err(ResolveError::Layout);
    }
    let package = runtime_dir.parent().ok_or(ResolveError::Layout)?;
    let cores = package.join("cores");
    let runtime_root = data_root.join("runtime");
    let generation_dir = runtime_root.join(format!("runtime-{generation:020}"));
    let xray = cores.join("xray");
    let mihomo = cores.join("mihomo");
    let xray_config = generation_dir.join("xray.json");
    let mihomo_config = generation_dir.join("mihomo.yaml");
    // The vendored dashboard is optional: absent in builds without one, present as an exact
    // package-root sibling of `cores` otherwise. Its presence is decided once, up front, so a
    // directory that appears only after that check can never be picked up mid-resolution.
    let dashboard = package.join("dashboard");
    let dashboard_present = fs::symlink_metadata(&dashboard).is_ok_and(|m| m.file_type().is_dir());

    let mut guards = Vec::new();
    for directory in [
        data_root,
        &runtime_root,
        &generation_dir,
        package,
        runtime_dir,
        &cores,
    ] {
        guard_chain(directory, true, owner_uid, &mut guards)?;
    }
    if dashboard_present {
        guard_chain(&dashboard, true, owner_uid, &mut guards)?;
    }
    for file in [
        host_executable,
        &xray,
        &mihomo,
        &xray_config,
        &mihomo_config,
    ] {
        guard_chain(file, false, owner_uid, &mut guards)?;
    }

    let canonical = |path: &Path| fs::canonicalize(path).map_err(|_| ResolveError::Missing);
    let canonical_package = canonical(package)?;
    let canonical_generation = canonical(&generation_dir)?;
    if !canonical(data_root).is_ok_and(|root| canonical_generation.starts_with(root)) {
        return Err(ResolveError::Escape);
    }
    for binary in [host_executable, &xray, &mihomo] {
        if !canonical(binary)?.starts_with(&canonical_package) {
            return Err(ResolveError::Escape);
        }
    }
    for config in [&xray_config, &mihomo_config] {
        if !canonical(config)?.starts_with(&canonical_generation) {
            return Err(ResolveError::Escape);
        }
    }
    let canonical_dashboard = if dashboard_present {
        let canonical_dashboard = canonical(&dashboard)?;
        if !canonical_dashboard.starts_with(&canonical_package) {
            return Err(ResolveError::Escape);
        }
        Some(canonical_dashboard)
    } else {
        None
    };
    for binary in [&xray, &mihomo] {
        let guard = guards
            .iter_mut()
            .rev()
            .find(|guard| guard.path == *binary)
            .ok_or(ResolveError::Missing)?;
        multicore_platform::elf::validate_elf64_x86_64(&mut guard.handle)
            .map_err(|_| ResolveError::InvalidElf)?;
    }
    Ok(ResolvedRuntime {
        paths: RuntimePaths {
            xray_binary: canonical(&xray)?,
            mihomo_binary: canonical(&mihomo)?,
            xray_config: canonical(&xray_config)?,
            mihomo_config: canonical(&mihomo_config)?,
            dashboard_directory: canonical_dashboard,
        },
        guards,
    })
}

fn guard_chain(
    path: &Path,
    target_directory: bool,
    owner_uid: u32,
    guards: &mut Vec<GuardedPath>,
) -> Result<(), ResolveError> {
    if !path.is_absolute() {
        return Err(ResolveError::Layout);
    }
    let mut ancestors = path.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.parent().is_none() || guards.iter().any(|guard| guard.path == ancestor) {
            continue;
        }
        let final_component = ancestor == path;
        let directory = !final_component || target_directory;
        guards.push(open_guard(ancestor, directory, final_component, owner_uid)?);
    }
    Ok(())
}

fn open_guard(
    path: &Path,
    directory: bool,
    final_component: bool,
    owner_uid: u32,
) -> Result<GuardedPath, ResolveError> {
    let flags = rustix::fs::OFlags::NOFOLLOW.bits() as i32
        | if directory {
            rustix::fs::OFlags::DIRECTORY.bits() as i32
        } else {
            0
        };
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|error| {
            if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                ResolveError::Symlink
            } else if error.kind() == std::io::ErrorKind::NotFound {
                ResolveError::Missing
            } else {
                ResolveError::Layout
            }
        })?;
    let metadata = handle.metadata().map_err(|_| ResolveError::Io)?;
    if metadata.is_dir() != directory || (!directory && !metadata.is_file()) {
        return Err(ResolveError::Layout);
    }
    if metadata.uid() != 0 && metadata.uid() != owner_uid {
        return Err(ResolveError::Ownership);
    }
    let mode = metadata.mode();
    let shared_sticky_root =
        directory && !final_component && metadata.uid() == 0 && mode & STICKY != 0;
    if mode & GROUP_OR_OTHER_WRITE != 0 && !shared_sticky_root {
        return Err(ResolveError::Ownership);
    }
    Ok(GuardedPath {
        path: path.to_path_buf(),
        identity: (metadata.dev(), metadata.ino()),
        handle,
    })
}
