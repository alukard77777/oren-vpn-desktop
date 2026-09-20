use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub(crate) struct DirectoryCapability {
    display_path: PathBuf,
    #[cfg(unix)]
    fd: rustix::fd::OwnedFd,
    #[cfg(windows)]
    handle: fs::File,
}

#[derive(Debug)]
pub(crate) struct CapabilityEntry {
    pub name: OsString,
    pub is_directory: bool,
}

#[derive(Debug)]
pub(crate) struct StoreLock {
    #[cfg(windows)]
    _file: fs::File,
}

impl DirectoryCapability {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        open_directory(path)
    }

    pub(crate) fn display_path(&self) -> &Path {
        &self.display_path
    }

    pub(crate) fn lock_exclusive(&self) -> io::Result<StoreLock> {
        #[cfg(unix)]
        {
            let duplicate = fd_into_file(rustix::io::dup(&self.fd)?);
            fs2::FileExt::try_lock_exclusive(&duplicate)?;
            Ok(StoreLock {})
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
        open_child(self, name)
    }

    pub(crate) fn create_child(&self, name: &OsStr) -> io::Result<Self> {
        validate_component(name)?;
        create_child(self, name)?;
        self.open_child(name)
    }

    pub(crate) fn file_metadata(&self, name: &OsStr) -> io::Result<Option<(bool, u64)>> {
        validate_component(name)?;
        file_metadata(self, name)
    }

    pub(crate) fn read_file(&self, name: &OsStr, max_bytes: u64) -> io::Result<Vec<u8>> {
        validate_component(name)?;
        let mut file = open_file_read(self, name)?;
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

    pub(crate) fn write_file(&self, name: &OsStr, bytes: &[u8]) -> io::Result<()> {
        validate_component(name)?;
        let mut file = open_file_create_new(self, name)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    }

    pub(crate) fn rename_child(&self, old: &Self, new_name: &OsStr) -> io::Result<()> {
        validate_component(new_name)?;
        rename_child(self, old, new_name)
    }

    pub(crate) fn replace_file(&self, old_name: &OsStr, new_name: &OsStr) -> io::Result<()> {
        validate_component(old_name)?;
        validate_component(new_name)?;
        replace_file(self, old_name, new_name)
    }

    pub(crate) fn remove_child_tree(&self, name: &OsStr) -> io::Result<()> {
        validate_component(name)?;
        remove_child_tree(self, name)
    }

    pub(crate) fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        validate_component(name)?;
        remove_file(self, name)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        sync_capability(self)
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
fn open_directory(path: &Path) -> io::Result<DirectoryCapability> {
    use rustix::fs::{Mode, OFlags, fchmod, open};
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    fchmod(&fd, Mode::from_raw_mode(0o700))?;
    Ok(DirectoryCapability {
        display_path: absolute_display_path(path)?,
        fd,
    })
}

#[cfg(unix)]
fn open_child(parent: &DirectoryCapability, name: &OsStr) -> io::Result<DirectoryCapability> {
    use rustix::fs::{Mode, OFlags, fchmod, openat};
    let fd = openat(
        &parent.fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    fchmod(&fd, Mode::from_raw_mode(0o700))?;
    Ok(DirectoryCapability {
        display_path: parent.display_path.join(name),
        fd,
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
fn open_file_read(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, openat};
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
    Ok(fd_into_file(fd))
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
fn sync_capability(directory: &DirectoryCapability) -> io::Result<()> {
    rustix::fs::fsync(&directory.fd)?;
    Ok(())
}

#[cfg(windows)]
fn open_directory(path: &Path) -> io::Result<DirectoryCapability> {
    open_windows_directory(path, false)
}

#[cfg(windows)]
fn open_child(parent: &DirectoryCapability, name: &OsStr) -> io::Result<DirectoryCapability> {
    open_windows_directory(&parent.display_path.join(name), true)
}

#[cfg(windows)]
fn open_windows_directory(path: &Path, removable: bool) -> io::Result<DirectoryCapability> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::GENERIC_READ,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
    };
    let handle = fs::OpenOptions::new()
        .read(true)
        .access_mode(GENERIC_READ | if removable { DELETE } else { 0 })
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
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
    super::snapshot::secure_directory(path)?;
    Ok(DirectoryCapability {
        display_path: absolute_display_path(path)?,
        handle,
    })
}

#[cfg(windows)]
fn create_child(parent: &DirectoryCapability, name: &OsStr) -> io::Result<()> {
    fs::create_dir(parent.display_path.join(name))
}

#[cfg(windows)]
fn entries(directory: &DirectoryCapability) -> io::Result<Vec<CapabilityEntry>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(&directory.display_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let is_directory = directory.open_child(&name).is_ok();
        result.push(CapabilityEntry { name, is_directory });
    }
    Ok(result)
}

#[cfg(windows)]
fn open_windows_file(
    directory: &DirectoryCapability,
    name: &OsStr,
    create_new: bool,
) -> io::Result<fs::File> {
    use std::{os::windows::fs::OpenOptionsExt, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            FILE_SHARE_WRITE, GetFileInformationByHandle,
        },
    };
    let path = directory.display_path.join(name);
    let access = if create_new {
        GENERIC_WRITE | DELETE
    } else {
        GENERIC_READ | DELETE
    };
    let file = fs::OpenOptions::new()
        .read(!create_new)
        .write(create_new)
        .create_new(create_new)
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
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
    super::snapshot::secure_file(&path)?;
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
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .access_mode(GENERIC_READ | GENERIC_WRITE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
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
    super::snapshot::secure_file(&path)?;
    Ok(file)
}

#[cfg(windows)]
fn file_metadata(directory: &DirectoryCapability, name: &OsStr) -> io::Result<Option<(bool, u64)>> {
    match open_windows_file(directory, name, false) {
        Ok(file) => Ok(Some((true, file.metadata()?.len()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(Some((false, 0))),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn open_file_read(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    open_windows_file(directory, name, false)
}

#[cfg(windows)]
fn open_file_create_new(directory: &DirectoryCapability, name: &OsStr) -> io::Result<fs::File> {
    open_windows_file(directory, name, true)
}

#[cfg(windows)]
fn rename_child(
    parent: &DirectoryCapability,
    old: &DirectoryCapability,
    new_name: &OsStr,
) -> io::Result<()> {
    rename_windows_handle(parent, &old.handle, new_name, false)
}

#[cfg(windows)]
fn replace_file(
    parent: &DirectoryCapability,
    old_name: &OsStr,
    new_name: &OsStr,
) -> io::Result<()> {
    let file = open_windows_file(parent, old_name, false)?;
    rename_windows_handle(parent, &file, new_name, true)
}

#[cfg(windows)]
fn rename_windows_handle(
    parent: &DirectoryCapability,
    source: &fs::File,
    new_name: &OsStr,
    replace: bool,
) -> io::Result<()> {
    use std::{os::windows::ffi::OsStrExt, os::windows::io::AsRawHandle, ptr};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };
    // The parent's retained handle denies delete sharing, so its display path cannot be
    // renamed or replaced while this absolute target name is consumed by the kernel.
    let target = parent.display_path.join(new_name);
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    let bytes = std::mem::size_of::<FILE_RENAME_INFO>() + name.len() * 2;
    let words = bytes.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = ptr::null_mut();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    if unsafe {
        SetFileInformationByHandle(
            source.as_raw_handle(),
            FileRenameInfo,
            buffer.as_ptr().cast(),
            bytes as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
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
    match parent.open_child(name) {
        Ok(child) => {
            for entry in child.entries()? {
                if entry.is_directory {
                    remove_child_tree(&child, &entry.name)?;
                } else {
                    let file = open_windows_file(&child, &entry.name, false)?;
                    mark_delete(&file)?;
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
    match open_windows_file(parent, name, false) {
        Ok(file) => mark_delete(&file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn sync_capability(_directory: &DirectoryCapability) -> io::Result<()> {
    Ok(())
}

fn absolute_display_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
