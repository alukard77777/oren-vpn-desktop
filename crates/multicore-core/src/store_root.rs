use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

#[cfg(test)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug)]
pub(crate) struct DirectoryCapability {
    display_path: PathBuf,
    repair_permissions: bool,
    #[cfg(unix)]
    fd: rustix::fd::OwnedFd,
    #[cfg(windows)]
    handle: fs::File,
    #[cfg(test)]
    sync_probe: Arc<SyncProbe>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct SyncProbe {
    fail_next: AtomicBool,
    calls: AtomicUsize,
}

#[derive(Debug, Clone)]
pub(crate) struct CapabilityFile {
    file: Arc<fs::File>,
    stable_path: Option<PathBuf>,
}

impl CapabilityFile {
    pub(crate) fn path(&self) -> Option<&Path> {
        self.stable_path.as_deref()
    }
}

#[derive(Debug)]
pub(crate) struct CapabilityEntry {
    pub name: OsString,
    pub is_directory: bool,
}

#[derive(Debug)]
pub(crate) struct StoreLock {
    #[cfg(any(unix, windows))]
    _file: fs::File,
}

impl DirectoryCapability {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        open_directory(path, true)
    }

    /// Opens an existing directory without creating entries or repairing ACLs/modes.
    pub(crate) fn open_readonly(path: &Path) -> io::Result<Self> {
        open_directory(path, false)
    }

    pub(crate) fn lock_exclusive(&self) -> io::Result<StoreLock> {
        self.require_mutable()?;
        #[cfg(unix)]
        {
            #[cfg(target_os = "solaris")]
            let file = open_unix_lock_file(self, OsStr::new(".index.lock"))?;
            #[cfg(not(target_os = "solaris"))]
            // Set CLOEXEC atomically so spawned children cannot retain the store lock.
            let file = fd_into_file(rustix::io::fcntl_dupfd_cloexec(&self.fd, 0)?);
            fs2::FileExt::try_lock_exclusive(&file)?;
            Ok(StoreLock { _file: file })
        }
        #[cfg(windows)]
        {
            let file = open_windows_lock_file(self, OsStr::new(".index.lock"))?;
            fs2::FileExt::try_lock_exclusive(&file)?;
            Ok(StoreLock { _file: file })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "directory capabilities are unsupported",
            ))
        }
    }

    pub(crate) fn entries(&self) -> io::Result<Vec<CapabilityEntry>> {
        entries(self)
    }

    pub(crate) fn open_child(&self, name: &OsStr) -> io::Result<Self> {
        validate_component(name)?;
        open_child(self, name, self.repair_permissions, false)
    }

    pub(crate) fn create_child(&self, name: &OsStr) -> io::Result<Self> {
        self.require_mutable()?;
        validate_component(name)?;
        create_child(self, name)?;
        self.open_child(name)
    }

    /// Like `create_child`, but the returned capability may later be passed
    /// to `rename_child` as the source. Only such a staging capability holds
    /// DELETE access: Windows 10 refuses to rename a directory with os error
    /// 32 while its *parent* is held open with DELETE access (Windows 11
    /// allows it), so every other long-lived directory handle - in
    /// particular each store root that staging directories are published
    /// into - must be opened without it.
    pub(crate) fn create_child_for_rename(&self, name: &OsStr) -> io::Result<Self> {
        self.require_mutable()?;
        validate_component(name)?;
        create_child(self, name)?;
        open_child(self, name, self.repair_permissions, true)
    }

    pub(crate) fn file_metadata(&self, name: &OsStr) -> io::Result<Option<(bool, u64)>> {
        validate_component(name)?;
        file_metadata(self, name)
    }

    pub(crate) fn read_file(&self, name: &OsStr, max_bytes: u64) -> io::Result<Vec<u8>> {
        validate_component(name)?;
        let mut file = open_file_read(self, name, self.repair_permissions)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capability-relative file exceeds size limit",
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn read_file_retained(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<(Vec<u8>, CapabilityFile)> {
        validate_component(name)?;
        let mut file = open_file_read(self, name, self.repair_permissions)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capability-relative file exceeds size limit",
            ));
        }
        let stable_path = stable_file_path(self, name, &file);
        Ok((
            bytes,
            CapabilityFile {
                file: Arc::new(file),
                stable_path,
            },
        ))
    }

    pub(crate) fn write_file(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(name)?;
        let mut file = open_file_create_new(self, name)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    }

    pub(crate) fn rename_child(&self, old: &Self, new_name: &OsStr) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(new_name)?;
        rename_child(self, old, new_name)
    }

    pub(crate) fn replace_file(&self, old_name: &OsStr, new_name: &OsStr) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(old_name)?;
        validate_component(new_name)?;
        replace_file(self, old_name, new_name)
    }

    pub(crate) fn remove_child_tree(&self, name: &OsStr) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(name)?;
        remove_child_tree(self, name)
    }

    pub(crate) fn remove_entry_nofollow(&self, name: &OsStr) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(name)?;
        remove_entry_nofollow(self, name)
    }

    pub(crate) fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(name)?;
        remove_file(self, name)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.require_mutable()?;
        #[cfg(test)]
        {
            self.sync_probe.calls.fetch_add(1, Ordering::SeqCst);
            if self.sync_probe.fail_next.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected directory sync failure"));
            }
        }
        sync_capability(self)
    }

    pub(crate) fn secure_regular_file(&self, name: &OsStr) -> io::Result<()> {
        self.require_mutable()?;
        validate_component(name)?;
        drop(open_file_read(self, name, self.repair_permissions)?);
        Ok(())
    }

    fn require_mutable(&self) -> io::Result<()> {
        if self.repair_permissions {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read-only directory capability cannot mutate storage",
            ))
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_sync_failure(&self) {
        self.sync_probe.fail_next.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn sync_call_count(&self) -> usize {
        self.sync_probe.calls.load(Ordering::SeqCst)
    }
}

impl PartialEq for CapabilityFile {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.file, &other.file) || self.stable_path == other.stable_path
    }
}

fn validate_component(name: &OsStr) -> io::Result<()> {
    let path = Path::new(name);
    if path.components().count() != 1 || matches!(name.to_str(), Some("." | "..") | None) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "capability path must be one UTF-8 component",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path, repair_permissions: bool) -> io::Result<DirectoryCapability> {
    use rustix::fs::{Mode, OFlags, fchmod, open};
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if repair_permissions {
        fchmod(&fd, Mode::from_raw_mode(0o700))?;
    }
    Ok(DirectoryCapability {
        display_path: absolute_display_path(path)?,
        repair_permissions,
        fd,
        #[cfg(test)]
        sync_probe: Arc::default(),
    })
}

#[cfg(unix)]
fn open_child(
    parent: &DirectoryCapability,
    name: &OsStr,
    repair_permissions: bool,
    _delete_access: bool,
) -> io::Result<DirectoryCapability> {
    use rustix::fs::{Mode, OFlags, fchmod, openat};
    let fd = openat(
        &parent.fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    if repair_permissions {
        fchmod(&fd, Mode::from_raw_mode(0o700))?;
    }
    Ok(DirectoryCapability {
        display_path: parent.display_path.join(name),
        repair_permissions,
        fd,
        #[cfg(test)]
        sync_probe: Arc::default(),
    })
}

#[cfg(unix)]
fn create_child(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    rustix::fs::mkdirat(&parent.fd, name, rustix::fs::Mode::from_raw_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
fn entries(directory: &DirectoryCapability) -> io::Result<Vec<CapabilityEntry>> {
    use rustix::fs::{AtFlags, FileType, statat};
    use std::os::unix::ffi::OsStrExt;
    let mut reader = rustix::fs::Dir::read_from(&directory.fd)?;
    let mut result = Vec::new();
    for entry in &mut reader {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes()).to_owned();
        if name == "." || name == ".." {
            continue;
        }
        let stat = statat(&directory.fd, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        let is_directory = FileType::from_raw_mode(stat.st_mode) == FileType::Directory;
        result.push(CapabilityEntry { name, is_directory });
    }
    Ok(result)
}

#[cfg(unix)]
fn file_metadata(directory: &DirectoryCapability, name: &OsStr) -> io::Result<Option<(bool, u64)>> {
    use rustix::fs::{AtFlags, FileType, statat};
    match statat(&directory.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some((
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
            stat.st_size as u64,
        ))),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn fd_into_file(fd: rustix::fd::OwnedFd) -> fs::File {
    use std::os::fd::{FromRawFd, IntoRawFd};
    unsafe { fs::File::from_raw_fd(fd.into_raw_fd()) }
}

#[cfg(unix)]
fn open_file_read(
    directory: &DirectoryCapability,
    name: &OsStr,
    repair_permissions: bool,
) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, fchmod, openat};
    let fd = openat(
        &directory.fd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = rustix::fs::fstat(&fd)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    if repair_permissions {
        fchmod(&fd, Mode::from_raw_mode(0o600))?;
    }
    Ok(fd_into_file(fd))
}

#[cfg(unix)]
fn stable_file_path(
    _directory: &DirectoryCapability,
    _name: &OsStr,
    file: &fs::File,
) -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsRawFd;
        let fd_root = format!("/proc/{}/fd", std::process::id());
        Some(PathBuf::from(fd_root).join(file.as_raw_fd().to_string()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = file;
        // /dev/fd is process-relative and this path is handed to a separate UI process.
        // Suppress the optional logo instead of exposing a path that could name unrelated bytes.
        None
    }
}

#[cfg(unix)]
fn open_file_create_new(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, fchmod, openat};
    let fd = openat(
        &directory.fd,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    fchmod(&fd, Mode::from_raw_mode(0o600))?;
    Ok(fd_into_file(fd))
}

#[cfg(all(unix, target_os = "solaris"))]
fn open_unix_lock_file(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, fchmod, openat};
    let fd = openat(
        &directory.fd,
        name,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let stat = rustix::fs::fstat(&fd)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "profile lock is not a regular file",
        ));
    }
    fchmod(&fd, Mode::from_raw_mode(0o600))?;
    Ok(fd_into_file(fd))
}

#[cfg(unix)]
fn rename_child(
    parent: &DirectoryCapability,
    old: &DirectoryCapability,
    new_name: &OsStr,
) -> io::Result<()> {
    let old_name = old
        .display_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child has no name"))?;
    rustix::fs::renameat(&parent.fd, old_name, &parent.fd, new_name)?;
    Ok(())
}

#[cfg(unix)]
fn replace_file(
    parent: &DirectoryCapability,
    old_name: &OsStr,
    new_name: &OsStr,
) -> io::Result<()> {
    rustix::fs::renameat(&parent.fd, old_name, &parent.fd, new_name)?;
    Ok(())
}

#[cfg(unix)]
fn remove_child_tree(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    use rustix::fs::AtFlags;
    match parent.open_child(name) {
        Ok(child) => {
            for entry in child.entries()? {
                if entry.is_directory {
                    remove_child_tree(&child, &entry.name)?;
                } else {
                    rustix::fs::unlinkat(&child.fd, &entry.name, AtFlags::empty())?;
                }
            }
            drop(child);
            rustix::fs::unlinkat(&parent.fd, name, AtFlags::REMOVEDIR)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(unix)]
fn remove_file(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    match rustix::fs::unlinkat(&parent.fd, name, rustix::fs::AtFlags::empty()) {
        Ok(()) => Ok(()),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn remove_entry_nofollow(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, statat};
    match statat(&parent.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => {
            remove_child_tree(parent, name)
        }
        Ok(_) => remove_file(parent, name),
        Err(error) if error == rustix::io::Errno::NOENT => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_capability(directory: &DirectoryCapability) -> io::Result<()> {
    rustix::fs::fsync(&directory.fd)?;
    Ok(())
}

#[cfg(windows)]
fn open_directory(path: &Path, repair_permissions: bool) -> io::Result<DirectoryCapability> {
    open_windows_directory(path, false, repair_permissions)
}

#[cfg(windows)]
fn open_child(
    parent: &DirectoryCapability,
    name: &OsStr,
    repair_permissions: bool,
    delete_access: bool,
) -> io::Result<DirectoryCapability> {
    open_windows_directory(
        &parent.display_path.join(name),
        delete_access && repair_permissions,
        repair_permissions,
    )
}

#[cfg(windows)]
fn open_windows_directory(
    path: &Path,
    removable: bool,
    repair_permissions: bool,
) -> io::Result<DirectoryCapability> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
    };
    let access = GENERIC_READ
        | if repair_permissions { GENERIC_WRITE } else { 0 }
        | if removable { DELETE } else { 0 };
    let handle = retry_on_transient_windows_lock("open directory", path, false, || {
        fs::OpenOptions::new()
            .read(true)
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH,
            )
            .open(path)
    })?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "directory capability target is reparse-backed or not a directory",
        ));
    }
    if repair_permissions {
        retry_on_transient_windows_lock("secure directory", path, false, || {
            super::snapshot::secure_directory(path)
        })?;
    }
    Ok(DirectoryCapability {
        display_path: absolute_display_path(path)?,
        repair_permissions,
        handle,
        #[cfg(test)]
        sync_probe: Arc::default(),
    })
}

#[cfg(windows)]
fn stable_file_path(
    directory: &DirectoryCapability,
    name: &OsStr,
    _file: &fs::File,
) -> Option<PathBuf> {
    // The retained file handle denies delete sharing, so this name cannot be replaced
    // while a Snapshot exposes it.
    Some(directory.display_path.join(name))
}

#[cfg(windows)]
fn create_child(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    fs::create_dir(parent.display_path.join(name))
}

#[cfg(windows)]
fn entries(directory: &DirectoryCapability) -> io::Result<Vec<CapabilityEntry>> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    let mut result = Vec::new();
    for entry in fs::read_dir(&directory.display_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(entry.path())?;
        let is_directory =
            metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0;
        result.push(CapabilityEntry { name, is_directory });
    }
    Ok(result)
}

#[cfg(windows)]
fn open_windows_file(
    directory: &DirectoryCapability,
    name: &OsStr,
    create_new: bool,
    mutation: bool,
    repair_permissions: bool,
) -> io::Result<fs::File> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
    };
    let path = directory.display_path.join(name);
    let access = if create_new || mutation {
        if create_new {
            GENERIC_WRITE | DELETE
        } else {
            GENERIC_READ | GENERIC_WRITE | DELETE
        }
    } else {
        GENERIC_READ
    };
    let operation_name = if create_new { "create" } else { "open" };
    let file = retry_on_transient_windows_lock(operation_name, &path, false, || {
        fs::OpenOptions::new()
            .read(!create_new)
            .write(create_new)
            .create_new(create_new)
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            // BACKUP_SEMANTICS lets an existing *directory* at this name open
            // too, so the attribute check below reports it as "not a regular
            // file" instead of CreateFile failing with ERROR_ACCESS_DENIED -
            // which the lock retry would otherwise wait out for seconds.
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH | FILE_FLAG_BACKUP_SEMANTICS,
            )
            .open(&path)
    })?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    if repair_permissions {
        retry_on_transient_windows_lock("secure", &path, false, || {
            super::snapshot::secure_file(&path)
        })?;
    }
    Ok(file)
}

#[cfg(windows)]
fn open_windows_lock_file(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetFileInformationByHandle,
        },
    };
    let path = directory.display_path.join(name);
    let file = retry_on_transient_windows_lock("open lock", &path, false, || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .access_mode(GENERIC_READ | GENERIC_WRITE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&path)
    })?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "profile lock is not a regular file",
        ));
    }
    retry_on_transient_windows_lock("secure lock", &path, false, || {
        super::snapshot::secure_file(&path)
    })?;
    Ok(file)
}

#[cfg(windows)]
fn file_metadata(directory: &DirectoryCapability, name: &OsStr) -> io::Result<Option<(bool, u64)>> {
    match open_windows_file(directory, name, false, false, directory.repair_permissions) {
        Ok(file) => Ok(Some((true, file.metadata()?.len()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(Some((false, 0))),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn open_file_read(
    directory: &DirectoryCapability,
    name: &OsStr,
    repair_permissions: bool,
) -> io::Result<fs::File> {
    open_windows_file(directory, name, false, false, repair_permissions)
}

#[cfg(windows)]
fn open_file_create_new(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    open_windows_file(directory, name, true, true, true)
}

#[cfg(windows)]
fn rename_child(
    parent: &DirectoryCapability,
    old: &DirectoryCapability,
    new_name: &OsStr,
) -> io::Result<()> {
    rename_windows_handle(parent, &old.handle, &old.display_path, new_name, false)
}

#[cfg(windows)]
fn replace_file(
    parent: &DirectoryCapability,
    old_name: &OsStr,
    new_name: &OsStr,
) -> io::Result<()> {
    let file = open_windows_file(parent, old_name, false, true, true)?;
    rename_windows_handle(
        parent,
        &file,
        &parent.display_path.join(old_name),
        new_name,
        true,
    )
}

#[cfg(windows)]
fn rename_windows_handle(
    parent: &DirectoryCapability,
    source: &fs::File,
    source_path: &Path,
    new_name: &OsStr,
    replace: bool,
) -> io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };
    // The retained parent handle denies delete sharing, so this absolute path cannot be
    // redirected by replacing the capability root while the kernel consumes it.
    let target = parent.display_path.join(new_name);
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let bytes = std::mem::size_of::<FILE_RENAME_INFO>() + name.len() * 2;
    let words = bytes.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = replace;
        // The source handle was opened with FILE_FLAG_WRITE_THROUGH, so successful rename
        // completion is the Windows metadata durability barrier.
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    retry_on_transient_windows_lock("rename", source_path, true, || {
        if unsafe {
            SetFileInformationByHandle(
                source.as_raw_handle(),
                FileRenameInfo,
                buffer.as_ptr().cast(),
                bytes as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if replace && target.is_dir() {
                // Replacing an existing directory is refused with the same
                // os error 5 a held child produces, but it never clears up
                // on its own - fail it immediately instead of retrying.
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, error));
            }
            Err(error)
        } else {
            Ok(())
        }
    })
}

/// Every Windows 10 machine (and no Windows 11 one) failed subscription
/// import with os error 32 (ERROR_SHARING_VIOLATION) inside snapshot
/// persistence. Measured on real Windows: renaming a directory while some
/// process holds a file inside it fails with os error 5, never 32, so the
/// 32 comes from *opening* a file or directory another process is holding,
/// consistent with an OS component (most likely the built-in real-time
/// scanner) that behaves differently between the two Windows versions. A
/// first rename-only 500ms retry did not help, which fits: it retried the
/// wrong operation. Every store open and rename now goes through here, is
/// retried for a bounded time, and - if the lock outlives the budget - the
/// error names the operation, the entry, and which processes Windows'
/// Restart Manager reports as holding it, so a remaining failure says
/// exactly who is responsible instead of just "file is in use".
#[cfg(windows)]
fn retry_on_transient_windows_lock<T>(
    operation_name: &'static str,
    target: &Path,
    retry_access_denied: bool,
    operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    const MAX_ATTEMPTS: u32 = 40;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(125);
    retry_on_transient_windows_lock_with_budget(
        operation_name,
        target,
        retry_access_denied,
        MAX_ATTEMPTS,
        RETRY_DELAY,
        operation,
    )
}

/// Same as `retry_on_transient_windows_lock`, but with the attempt count
/// and delay as parameters so tests can exercise the "retries are
/// eventually exhausted" path without taking the full production budget.
#[cfg(windows)]
fn retry_on_transient_windows_lock_with_budget<T>(
    operation_name: &'static str,
    target: &Path,
    retry_access_denied: bool,
    max_attempts: u32,
    retry_delay: std::time::Duration,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_ACCESS_DENIED: i32 = 5;
    let started = std::time::Instant::now();
    for attempt in 1..=max_attempts {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                // Opens only ever reported 32 for a held entry; a 5 there is a
                // real permission problem and must fail fast. A directory
                // rename reports a held child as 5, so only rename retries it.
                let is_retryable = match error.raw_os_error() {
                    Some(ERROR_SHARING_VIOLATION) => true,
                    Some(ERROR_ACCESS_DENIED) => retry_access_denied,
                    _ => false,
                };
                if !is_retryable {
                    return Err(error);
                }
                if attempt == max_attempts {
                    return Err(io::Error::new(
                        error.kind(),
                        StillLockedError {
                            operation: operation_name,
                            entry: target
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            attempts: attempt,
                            elapsed: started.elapsed(),
                            holders: processes_holding(target),
                            source: error,
                        },
                    ));
                }
                std::thread::sleep(retry_delay);
            }
        }
    }
    unreachable!("loop always returns on its last attempt")
}

/// Carries only the entry's own name (never the full path, which contains
/// the Windows user name) and process names/PIDs, so it stays as safe to
/// log as the rest of the event it ends up in.
#[cfg(windows)]
#[derive(Debug)]
struct StillLockedError {
    operation: &'static str,
    entry: String,
    attempts: u32,
    elapsed: std::time::Duration,
    holders: Vec<String>,
    source: io::Error,
}

#[cfg(windows)]
impl std::fmt::Display for StillLockedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} '{}' still blocked after {} attempts over {} ms; held by: {}",
            self.operation,
            self.entry,
            self.attempts,
            self.elapsed.as_millis(),
            if self.holders.is_empty() {
                "no process reported by Restart Manager".to_owned()
            } else {
                self.holders.join(", ")
            }
        )
    }
}

#[cfg(windows)]
impl std::error::Error for StillLockedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Asks Windows' Restart Manager which processes hold `target` open (or,
/// for a directory, any file directly inside it - Restart Manager only
/// tracks files). Best-effort: any failure just yields an empty list.
#[cfg(windows)]
fn processes_holding(target: &Path) -> Vec<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::RestartManager::{
        CCH_RM_SESSION_KEY, RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
        RmStartSession,
    };
    const ERROR_MORE_DATA: u32 = 234;
    const MAX_PROBED_FILES: usize = 32;

    let files: Vec<PathBuf> = if target.is_dir() {
        fs::read_dir(target)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file())
                    .take(MAX_PROBED_FILES)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![target.to_path_buf()]
    };
    if files.is_empty() {
        return Vec::new();
    }
    let wide: Vec<Vec<u16>> = files
        .iter()
        .map(|path| path.as_os_str().encode_wide().chain(Some(0)).collect())
        .collect();
    let names: Vec<*const u16> = wide.iter().map(|name| name.as_ptr()).collect();

    let mut session = 0_u32;
    let mut key = [0_u16; CCH_RM_SESSION_KEY as usize + 1];
    if unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) } != 0 {
        return Vec::new();
    }
    let mut holders = Vec::new();
    let registered = unsafe {
        RmRegisterResources(
            session,
            names.len() as u32,
            names.as_ptr(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
        )
    } == 0;
    if registered {
        let mut needed = 0_u32;
        let mut count = 0_u32;
        let mut reasons = 0_u32;
        let status = unsafe {
            RmGetList(
                session,
                &mut needed,
                &mut count,
                std::ptr::null_mut(),
                &mut reasons,
            )
        };
        if status == ERROR_MORE_DATA && needed > 0 {
            let mut infos = vec![RM_PROCESS_INFO::default(); needed as usize];
            count = needed;
            let status = unsafe {
                RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    infos.as_mut_ptr(),
                    &mut reasons,
                )
            };
            if status == 0 {
                for info in infos.iter().take(count as usize) {
                    let wide_name = |raw: &[u16]| {
                        let length = raw.iter().position(|&unit| unit == 0).unwrap_or(raw.len());
                        String::from_utf16_lossy(&raw[..length])
                    };
                    let app = wide_name(&info.strAppName);
                    let service = wide_name(&info.strServiceShortName);
                    holders.push(if service.is_empty() {
                        format!("{app} (pid {})", info.Process.dwProcessId)
                    } else {
                        format!("{app} [{service}] (pid {})", info.Process.dwProcessId)
                    });
                }
            }
        }
    }
    unsafe { RmEndSession(session) };
    holders
}

#[cfg(windows)]
fn mark_delete(file: &fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn remove_child_tree(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    validate_component(name)?;
    match open_child(parent, name, parent.repair_permissions, true) {
        Ok(child) => {
            for entry in child.entries()? {
                if entry.is_directory {
                    remove_child_tree(&child, &entry.name)?;
                } else {
                    remove_entry_nofollow(&child, &entry.name)?;
                }
            }
            mark_delete(&child.handle)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(windows)]
fn remove_file(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    match open_windows_file(parent, name, false, true, true) {
        Ok(file) => mark_delete(&file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn remove_entry_nofollow(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    let path = parent.display_path.join(name);
    let file = match fs::OpenOptions::new()
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH,
        )
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
        && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
    {
        drop(file);
        return remove_child_tree(parent, name);
    }
    mark_delete(&file)
}

#[cfg(windows)]
fn sync_capability(directory: &DirectoryCapability) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::ERROR_INVALID_HANDLE, Storage::FileSystem::FlushFileBuffers,
    };
    if unsafe { FlushFileBuffers(directory.handle.as_raw_handle()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_HANDLE as i32) {
            // FlushFileBuffers is not supported for directory handles on every Windows
            // filesystem. Publication remains durable because all capability-relative
            // rename source handles use FILE_FLAG_WRITE_THROUGH.
            Ok(())
        } else {
            Err(error)
        }
    } else {
        Ok(())
    }
}

fn absolute_display_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn exclusive_lock_is_close_on_exec() {
        let temp = tempfile::tempdir().unwrap();
        let directory = DirectoryCapability::open(temp.path()).unwrap();
        let lock = directory.lock_exclusive().unwrap();

        let flags = rustix::io::fcntl_getfd(&lock._file).unwrap();
        assert!(
            flags.contains(rustix::io::FdFlags::CLOEXEC),
            "a spawned child must not inherit the store lock"
        );
    }

    #[test]
    fn opening_existing_regular_file_restores_owner_only_mode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config");
        fs::write(&path, b"secret").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let directory = DirectoryCapability::open(temp.path()).unwrap();
        assert_eq!(
            directory.read_file(OsStr::new("config"), 32).unwrap(),
            b"secret"
        );
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(all(test, windows))]
mod windows_rename_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn inner_os_error(error: &io::Error) -> Option<i32> {
        error.raw_os_error().or_else(|| {
            std::error::Error::source(error)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .and_then(io::Error::raw_os_error)
        })
    }

    /// Regression coverage for the Windows-10-only subscription import
    /// failure (os error 32 on every import): Windows 10 refuses to rename a
    /// directory while its parent is held open with DELETE access, and
    /// every profile store root was such a parent. Proven with a minimal
    /// repro on Windows 10 19045 (fails) vs Windows 11 26200 (succeeds), so
    /// this pins the invariant that is checkable on any Windows version:
    /// ordinary directory capabilities carry no DELETE access - only a
    /// staging capability that is itself renamed does.
    #[test]
    fn only_rename_staging_capabilities_hold_delete_access() {
        let temp = tempfile::tempdir().unwrap();
        let root = DirectoryCapability::open(temp.path()).unwrap();
        let profile = root.create_child(OsStr::new("profile")).unwrap();
        let reopened = root.open_child(OsStr::new("profile")).unwrap();
        for capability in [&root, &profile, &reopened] {
            let error = mark_delete(&capability.handle)
                .expect_err("a store root handle must not be able to delete its directory");
            assert_eq!(error.raw_os_error(), Some(5), "{error}");
        }

        let staging = profile
            .create_child_for_rename(OsStr::new("staging"))
            .unwrap();
        staging.write_file(OsStr::new("mihomo.yaml"), b"x").unwrap();
        let started = Instant::now();
        profile
            .rename_child(&staging, OsStr::new("snapshot-1"))
            .expect("publishing into a store root must succeed");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "rename needed retries"
        );
        assert!(
            temp.path()
                .join("profile")
                .join("snapshot-1")
                .join("mihomo.yaml")
                .is_file()
        );

        // Removal still works: it opens its own DELETE handle (once nothing
        // else holds the directory, as in production).
        drop(staging);
        drop(reopened);
        drop(profile);
        root.remove_child_tree(OsStr::new("profile")).unwrap();
        assert!(!temp.path().join("profile").exists());
    }

    /// A held child file blocks renaming its directory; the rename must
    /// succeed as soon as the other handle goes away instead of failing
    /// the whole import outright.
    #[test]
    fn rename_survives_a_transient_lock_from_another_handle() {
        let temp = tempfile::tempdir().unwrap();
        let root = DirectoryCapability::open(temp.path()).unwrap();
        let staging = root.create_child_for_rename(OsStr::new("staging")).unwrap();
        staging
            .write_file(OsStr::new("mihomo.yaml"), b"placeholder")
            .unwrap();

        let blocking_path = temp.path().join("staging").join("mihomo.yaml");
        let blocking_handle = fs::File::open(&blocking_path).unwrap();
        let released_at = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
        let released_at_writer = released_at.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(blocking_handle);
            *released_at_writer.lock().unwrap() = Some(Instant::now());
        });

        let started = Instant::now();
        root.rename_child(&staging, OsStr::new("published"))
            .expect("rename must succeed once the other handle releases its lock");
        let finished = Instant::now();
        releaser.join().unwrap();

        assert!(finished >= released_at.lock().unwrap().expect("releaser ran"));
        assert!(
            finished - started < Duration::from_secs(2),
            "{:?}",
            finished - started
        );
        assert!(temp.path().join("published").join("mihomo.yaml").is_file());
    }

    /// Regression coverage for the real Windows-10-only os error 32: an
    /// open of an entry another process holds exclusively is a sharing
    /// violation. It must be retried, and once the budget is exhausted the
    /// error must name the operation, the entry, and the holding process
    /// (here: this very test process, found via Restart Manager), while
    /// keeping the original OS error reachable as its source.
    #[test]
    fn an_exhausted_lock_names_the_operation_entry_and_holding_process() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mihomo.yaml");
        fs::write(&path, b"placeholder").unwrap();
        let _exclusive = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .unwrap()
        };

        let started = Instant::now();
        let error = retry_on_transient_windows_lock_with_budget(
            "open",
            &path,
            false,
            3,
            Duration::from_millis(20),
            || fs::File::open(&path),
        )
        .expect_err("an exclusively held file cannot be opened");

        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "must actually retry"
        );
        assert_eq!(inner_os_error(&error), Some(32), "{error}");
        let text = error.to_string();
        assert!(
            text.contains("open 'mihomo.yaml' still blocked after 3 attempts"),
            "{text}"
        );
        assert!(
            text.contains(&format!("(pid {})", std::process::id())),
            "Restart Manager must report this test process as the holder: {text}"
        );
        assert!(
            !text.contains(&temp.path().display().to_string()),
            "full path leaked: {text}"
        );
    }

    #[test]
    fn non_lock_errors_are_returned_immediately_and_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.yaml");
        let mut calls = 0;
        let error = retry_on_transient_windows_lock_with_budget(
            "open",
            &missing,
            false,
            5,
            Duration::from_millis(20),
            || {
                calls += 1;
                fs::File::open(&missing)
            },
        )
        .expect_err("missing file");
        assert_eq!(calls, 1);
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
