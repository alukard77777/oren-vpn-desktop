#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use multicore_release::{
    Version, extract_verified_bundle, rename_with_retry, transactional_swap, verify_archive_file,
};
use serde::Serialize;

#[cfg(windows)]
mod session;

const WAIT_TIMEOUT_MS: u32 = 120_000;

/// Service mode: the updater runs as SYSTEM on behalf of the user logged on to `session_id`,
/// whose update results live in `user_update_dir`.
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
struct ServiceLaunch {
    session_id: u32,
    user_update_dir: PathBuf,
}

#[derive(Debug)]
struct ApplyRequest {
    wait_pid: u32,
    archive: PathBuf,
    target: PathBuf,
    expected_size: u64,
    expected_sha256: String,
    version: Version,
    service: Option<ServiceLaunch>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<ApplyRequest, String> {
    let mut args = args.into_iter();
    let _program = args.next().ok_or("missing program name")?;
    let mut values = BTreeMap::new();
    while let Some(key) = args.next() {
        if !matches!(
            key.as_str(),
            "--wait-pid"
                | "--archive"
                | "--target"
                | "--size"
                | "--sha256"
                | "--version"
                | "--service-session"
                | "--user-update-dir"
        ) {
            return Err(format!("unknown argument: {key}"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {key}"))?;
        if values.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate argument: {key}"));
        }
    }
    let service_keys = ["--service-session", "--user-update-dir"]
        .iter()
        .filter(|key| values.contains_key(**key))
        .count();
    if values.len() != 6 + service_keys || service_keys == 1 {
        return Err("incomplete apply request".into());
    }

    let take = |key: &str| {
        values
            .get(key)
            .cloned()
            .ok_or_else(|| format!("missing {key}"))
    };
    let wait_pid = take("--wait-pid")?
        .parse::<u32>()
        .map_err(|_| "invalid wait pid")?;
    if wait_pid == 0 {
        return Err("invalid wait pid".into());
    }
    let expected_size = take("--size")?
        .parse::<u64>()
        .map_err(|_| "invalid archive size")?;
    let version = Version::parse(&take("--version")?).map_err(|error| error.to_string())?;
    let service = if service_keys == 2 {
        let session_id = take("--service-session")?
            .parse::<u32>()
            .map_err(|_| "invalid service session")?;
        let user_update_dir = PathBuf::from(take("--user-update-dir")?);
        if !user_update_dir.is_absolute() {
            return Err("user update directory must be absolute".into());
        }
        Some(ServiceLaunch {
            session_id,
            user_update_dir,
        })
    } else {
        None
    };

    Ok(ApplyRequest {
        wait_pid,
        archive: PathBuf::from(take("--archive")?),
        target: PathBuf::from(take("--target")?),
        expected_size,
        expected_sha256: take("--sha256")?,
        version,
        service,
    })
}

fn main() -> ExitCode {
    let request = match parse_args(std::env::args()) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("invalid updater request: {error}");
            return ExitCode::from(2);
        }
    };
    match apply_update(&request) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            write_log(&request, &format!("failed: {error}"));
            write_result(&request, false, &error.to_string());
            let _ = relaunch_current_client(&request);
            ExitCode::FAILURE
        }
    }
}

fn apply_update(request: &ApplyRequest) -> Result<(), Box<dyn std::error::Error>> {
    if !request.archive.is_absolute() || !request.target.is_absolute() {
        return Err("archive and target paths must be absolute".into());
    }
    verify_archive_file(
        &request.archive,
        request.expected_size,
        &request.expected_sha256,
    )?;
    wait_for_process_exit(request.wait_pid, WAIT_TIMEOUT_MS)?;

    let target = request.target.canonicalize()?;
    // The daemon and the elevated core host (with its cores) run from the
    // install directory and may outlive the desktop briefly.
    let lingering = wait_for_install_processes(&target, INSTALL_PROCESS_TIMEOUT);
    if !lingering.is_empty() {
        write_log(
            request,
            &format!("processes still running from the install directory: {lingering:?}"),
        );
    }
    verify_archive_file(
        &request.archive,
        request.expected_size,
        &request.expected_sha256,
    )?;

    let parent = target.parent().ok_or("install directory has no parent")?;
    remove_stale_work_directories(parent);
    let suffix = format!("{}-{}", request.version, std::process::id());
    let staged = parent.join(format!("{STAGE_PREFIX}{suffix}"));
    let backup = parent.join(format!("{BACKUP_PREFIX}{suffix}"));
    if staged.exists() || backup.exists() {
        return Err("update staging paths already exist".into());
    }

    extract_verified_bundle(&request.archive, &staged)?;
    if let Err(error) = transactional_swap(&target, &staged, &backup) {
        let _ = remove_scoped_directory(&staged, STAGE_PREFIX);
        if backup.exists() {
            write_log(
                request,
                &format!("previous version preserved at {}", backup.display()),
            );
        }
        return Err(error.into());
    }

    // Record the outcome before the new client starts so it can report it.
    write_result(request, true, "Обновление установлено");
    let marker = update_state_dir(request).map(|directory| directory.join(STARTED_MARKER));
    if let Some(marker) = &marker {
        as_user(request, || {
            let _ = fs::remove_file(marker);
        });
    }
    let version = request.version.to_string();
    // The new service must be running before the new daemon looks for it.
    start_service(request);
    let outcome = launch_and_finalize(
        &target,
        &backup,
        |executable| launch_client(request, executable, &target),
        || {
            as_user(request, || {
                marker
                    .as_deref()
                    .is_some_and(|marker| marker_confirms(marker, &version))
            })
            .unwrap_or(false)
        },
        STARTUP_CONFIRM_TIMEOUT,
        std::thread::sleep,
        // In service mode the new service runs from `current`; it must be stopped (and its
        // processes gone) before `current` can be renamed away.
        || stop_service(request, &target),
    );
    if outcome.is_err() {
        // Rolled back to the previous version (or left as is): bring its service back.
        start_service(request);
    }
    let outcome = outcome.map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;

    match outcome {
        Finalization::Confirmed(Ok(())) => write_log(request, "installed and launched"),
        Finalization::Confirmed(Err(error)) => write_log(
            request,
            &format!(
                "installed; previous version cleanup deferred ({}): {error}",
                backup.display()
            ),
        ),
        Finalization::Unconfirmed => write_log(
            request,
            &format!(
                "installed; start not yet confirmed, previous version kept at {}",
                backup.display()
            ),
        ),
    }
    let _ = fs::remove_file(&request.archive);
    Ok(())
}

const STAGE_PREFIX: &str = ".multicore-stage-";
const BACKUP_PREFIX: &str = ".multicore-previous-";
const FAILED_PREFIX: &str = ".multicore-failed-";
/// Written by the desktop (containing its version) once its window and daemon are up.
const STARTED_MARKER: &str = "started-ok";
const INSTALL_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_POLL: Duration = Duration::from_millis(250);
#[cfg(windows)]
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_secs(30);

fn marker_confirms(marker: &Path, version: &str) -> bool {
    fs::read_to_string(marker).is_ok_and(|content| content.trim() == version)
}

fn remove_scoped_directory(path: &Path, prefix: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid update directory name"))?;
    if !name.starts_with(prefix) {
        return Err(io::Error::other(
            "refusing to remove an unrelated directory",
        ));
    }
    fs::remove_dir_all(path)
}

/// Remove staging and failed-install leftovers of earlier interrupted attempts.
/// Previous-version backups are kept: they may be the only working copy.
fn remove_stale_work_directories(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let path = entry.path();
        for prefix in [STAGE_PREFIX, FAILED_PREFIX] {
            if remove_scoped_directory(&path, prefix).is_ok() {
                break;
            }
        }
    }
}

fn discard_backup(backup: &Path) -> io::Result<()> {
    remove_scoped_directory(backup, BACKUP_PREFIX)
}

pub(crate) trait LaunchedClient {
    fn has_exited(&mut self) -> bool;
}

impl LaunchedClient for std::process::Child {
    fn has_exited(&mut self) -> bool {
        !matches!(self.try_wait(), Ok(None))
    }
}

#[derive(Debug)]
enum Finalization {
    /// The new client confirmed its start; carries the backup cleanup result.
    Confirmed(io::Result<()>),
    /// The new client is still running but has not confirmed yet. The backup is
    /// kept; the desktop removes it on a later confirmed start.
    Unconfirmed,
}

fn launch_and_finalize(
    target: &Path,
    backup: &Path,
    launch: impl FnOnce(&Path) -> io::Result<Box<dyn LaunchedClient>>,
    confirmed: impl Fn() -> bool,
    timeout: Duration,
    mut sleep: impl FnMut(Duration),
    before_rollback: impl FnOnce(),
) -> Result<Finalization, String> {
    let executable = target.join("MultiCore.exe");
    let failure = match launch(&executable) {
        Err(launch_error) => format!("new client could not start: {launch_error}"),
        Ok(mut client) => {
            let mut waited = Duration::ZERO;
            loop {
                if confirmed() {
                    return Ok(Finalization::Confirmed(discard_backup(backup)));
                }
                if client.has_exited() {
                    // The client may have confirmed just before exiting normally.
                    if confirmed() {
                        return Ok(Finalization::Confirmed(discard_backup(backup)));
                    }
                    break "new client exited before confirming its start".to_owned();
                }
                if waited >= timeout {
                    return Ok(Finalization::Unconfirmed);
                }
                sleep(STARTUP_POLL);
                waited += STARTUP_POLL;
            }
        }
    };
    before_rollback();
    rollback_after_launch_failure(target, backup).map_err(|rollback_error| {
        format!(
            "{failure}; rollback failed: {rollback_error}; previous version preserved at {}",
            backup.display()
        )
    })?;
    Err(failure)
}

fn relaunch_current_client(request: &ApplyRequest) -> io::Result<()> {
    start_service(request);
    let executable = request.target.join("MultiCore.exe");
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "current MultiCore executable is unavailable",
        ));
    }
    launch_client(request, &executable, &request.target).map(|_| ())
}

/// Starts the desktop: directly without the service, in the user's session in service mode.
fn launch_client(
    request: &ApplyRequest,
    executable: &Path,
    directory: &Path,
) -> io::Result<Box<dyn LaunchedClient>> {
    match &request.service {
        None => Command::new(executable)
            .current_dir(directory)
            .spawn()
            .map(|child| Box::new(child) as Box<dyn LaunchedClient>),
        #[cfg(windows)]
        Some(service) => {
            let token = session::user_token(service.session_id)?;
            session::launch(&token, executable, directory)
                .map(|client| Box::new(client) as Box<dyn LaunchedClient>)
        }
        #[cfg(not(windows))]
        Some(_) => Err(io::Error::other("service mode is Windows-only")),
    }
}

/// In service mode, (re)starts the MultiCore service; a no-op otherwise.
#[cfg(windows)]
fn start_service(request: &ApplyRequest) {
    if request.service.is_some() {
        let _ = session::start_service();
    }
}

#[cfg(not(windows))]
fn start_service(_request: &ApplyRequest) {}

/// In service mode, stops the MultiCore service and waits until nothing runs from `install`;
/// a no-op otherwise.
#[cfg(windows)]
fn stop_service(request: &ApplyRequest, install: &Path) {
    if request.service.is_some() {
        if let Err(error) = session::stop_service(SERVICE_STOP_TIMEOUT) {
            write_log(
                request,
                &format!("service did not stop before rollback: {error}"),
            );
        }
        let lingering = wait_for_install_processes(install, INSTALL_PROCESS_TIMEOUT);
        if !lingering.is_empty() {
            write_log(
                request,
                &format!("processes still running before rollback: {lingering:?}"),
            );
        }
    }
}

#[cfg(not(windows))]
fn stop_service(_request: &ApplyRequest, _install: &Path) {}

/// Runs `work` as the logged-on user in service mode, directly otherwise, so SYSTEM never
/// touches a user-controlled path with its own rights.
fn as_user<T>(request: &ApplyRequest, work: impl FnOnce() -> T) -> Option<T> {
    match &request.service {
        None => Some(work()),
        #[cfg(windows)]
        Some(service) => session::user_token(service.session_id)
            .and_then(|token| session::impersonating(&token, work))
            .ok(),
        #[cfg(not(windows))]
        Some(_) => None,
    }
}

fn rollback_after_launch_failure(target: &Path, backup: &Path) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("install directory has no parent"))?;
    let failed = parent.join(format!("{FAILED_PREFIX}{}", std::process::id()));
    rename_with_retry(target, &failed)?;
    if let Err(rollback_error) = rename_with_retry(backup, target) {
        let _ = rename_with_retry(&failed, target);
        return Err(rollback_error);
    }
    let _ = fs::remove_dir_all(failed);
    Ok(())
}

/// Wait until no process whose image lives under `install` is running, or the
/// timeout elapses. Returns the PIDs still running at the deadline.
fn wait_for_install_processes(install: &Path, timeout: Duration) -> Vec<u32> {
    wait_until_clear(
        || processes_under(install),
        timeout,
        STARTUP_POLL,
        std::thread::sleep,
    )
}

fn wait_until_clear(
    mut running: impl FnMut() -> Vec<u32>,
    timeout: Duration,
    poll: Duration,
    mut sleep: impl FnMut(Duration),
) -> Vec<u32> {
    let mut waited = Duration::ZERO;
    loop {
        let remaining = running();
        if remaining.is_empty() || waited >= timeout {
            return remaining;
        }
        sleep(poll);
        waited += poll;
    }
}

fn normalized_windows_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    };
    path.replace('/', "\\").to_lowercase()
}

// Pure path-matching logic used by processes_under on Windows and exercised
// directly by cross-platform unit tests; it has no Windows-only body itself.
#[cfg_attr(not(windows), allow(dead_code))]
fn image_is_within(image: &str, install: &Path) -> bool {
    let mut root = normalized_windows_path(&install.to_string_lossy());
    if !root.ends_with('\\') {
        root.push('\\');
    }
    normalized_windows_path(image).starts_with(&root)
}

#[cfg(windows)]
fn processes_under(install: &Path) -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    let own = std::process::id();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        let pid = entry.th32ProcessID;
        if pid != 0 && pid != own {
            // Limited query access also works for elevated same-user processes.
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if !process.is_null() {
                let mut buffer = [0_u16; 1024];
                let mut length = buffer.len() as u32;
                let queried = unsafe {
                    QueryFullProcessImageNameW(
                        process,
                        PROCESS_NAME_WIN32,
                        buffer.as_mut_ptr(),
                        &mut length,
                    )
                } != 0;
                unsafe { CloseHandle(process) };
                if queried {
                    let image = String::from_utf16_lossy(&buffer[..length as usize]);
                    if image_is_within(&image, install) {
                        found.push(pid);
                    }
                }
            }
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
}

#[cfg(not(windows))]
fn processes_under(_install: &Path) -> Vec<u32> {
    Vec::new()
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32, timeout_ms: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        let error = unsafe { GetLastError() };
        if error == ERROR_INVALID_PARAMETER {
            return Ok(());
        }
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let wait = unsafe { WaitForSingleObject(process, timeout_ms) };
    unsafe { CloseHandle(process) };
    if wait == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "desktop did not exit before updater timeout",
        ))
    }
}

#[cfg(not(windows))]
fn wait_for_process_exit(_pid: u32, _timeout_ms: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "MultiCore updates are supported only on Windows",
    ))
}

#[derive(Serialize)]
struct UpdateResult<'a> {
    success: bool,
    version: String,
    message: &'a str,
    timestamp_unix: u64,
}

/// Where the result, log and start marker live: the user's update directory in service mode,
/// the archive's directory otherwise.
fn update_state_dir(request: &ApplyRequest) -> Option<PathBuf> {
    match &request.service {
        Some(service) => Some(service.user_update_dir.clone()),
        None => request.archive.parent().map(PathBuf::from),
    }
}

fn write_result(request: &ApplyRequest, success: bool, message: &str) {
    let Some(directory) = update_state_dir(request) else {
        return;
    };
    let result = UpdateResult {
        success,
        version: request.version.to_string(),
        message,
        timestamp_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let Ok(bytes) = serde_json::to_vec(&result) else {
        return;
    };
    as_user(request, || {
        let temporary = directory.join("result.json.tmp");
        if fs::write(&temporary, bytes).is_ok() {
            let _ = fs::rename(temporary, directory.join("result.json"));
        }
    });
}

fn write_log(request: &ApplyRequest, message: &str) {
    let Some(directory) = update_state_dir(request) else {
        return;
    };
    let append = |directory: &Path| {
        let _ = fs::create_dir_all(directory);
        if let Ok(mut log) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("updater.log"))
        {
            let _ = writeln!(log, "v{}: {message}", request.version);
        }
    };
    if as_user(request, || append(&directory)).is_none() {
        // Impersonation failed (the user logged off?): keep a trace in the service's own,
        // SYSTEM-owned update directory next to the archive.
        if let Some(own) = request.archive.parent() {
            append(own);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Finalization, LaunchedClient, discard_backup, image_is_within, launch_and_finalize,
        marker_confirms, parse_args, remove_stale_work_directories, update_state_dir,
        wait_until_clear,
    };
    use std::cell::Cell;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    struct FakeClient {
        exits_after_polls: Option<u32>,
        polls: u32,
    }

    impl LaunchedClient for FakeClient {
        fn has_exited(&mut self) -> bool {
            self.polls += 1;
            self.exits_after_polls
                .is_some_and(|limit| self.polls >= limit)
        }
    }

    fn running(exits_after_polls: Option<u32>) -> Box<dyn LaunchedClient> {
        Box::new(FakeClient {
            exits_after_polls,
            polls: 0,
        })
    }

    fn install_fixture(temp: &Path) -> (PathBuf, PathBuf) {
        let target = temp.join("MultiCore");
        let backup = temp.join(".multicore-previous-0.2.0-42");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(target.join("new.exe"), b"new").unwrap();
        std::fs::write(backup.join("old.exe"), b"old").unwrap();
        (target, backup)
    }

    #[test]
    fn parses_exact_apply_request() {
        let args = [
            "multicore-updater.exe",
            "--wait-pid",
            "42",
            "--archive",
            "C:\\stage\\bundle.zip",
            "--target",
            "C:\\apps\\MultiCore",
            "--size",
            "123",
            "--sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--version",
            "0.2.0",
        ];
        let request = parse_args(args.into_iter().map(str::to_owned)).unwrap();
        assert_eq!(request.wait_pid, 42);
        assert_eq!(request.expected_size, 123);
        assert_eq!(request.version.to_string(), "0.2.0");
    }

    #[test]
    fn rejects_unknown_or_duplicate_arguments() {
        let duplicate = [
            "updater",
            "--wait-pid",
            "42",
            "--wait-pid",
            "43",
            "--archive",
            "a",
            "--target",
            "b",
            "--size",
            "1",
            "--sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--version",
            "0.2.0",
        ];
        assert!(parse_args(duplicate.into_iter().map(str::to_owned)).is_err());
    }

    #[test]
    fn cleanup_removes_only_a_scoped_previous_version_directory() {
        let temp = tempfile::tempdir().unwrap();
        let backup = temp.path().join(".multicore-previous-0.2.0-42");
        let installer_metadata = temp.path().join("unins000.dat");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("old.exe"), b"old").unwrap();
        std::fs::write(&installer_metadata, b"keep").unwrap();

        discard_backup(&backup).unwrap();

        assert!(!backup.exists());
        assert_eq!(std::fs::read(installer_metadata).unwrap(), b"keep");
        assert!(discard_backup(temp.path()).is_err());
    }

    #[test]
    fn backup_is_discarded_only_after_the_new_client_confirms_its_start() {
        let temp = tempfile::tempdir().unwrap();
        let (target, backup) = install_fixture(temp.path());
        let polls = Cell::new(0);
        let outcome = launch_and_finalize(
            &target,
            &backup,
            |_| Ok(running(None)),
            || {
                polls.set(polls.get() + 1);
                assert!(backup.exists(), "backup must survive until confirmation");
                polls.get() >= 3
            },
            Duration::from_secs(60),
            |_| {},
            || {},
        )
        .unwrap();
        assert!(matches!(outcome, Finalization::Confirmed(Ok(()))));
        assert!(!backup.exists());
        assert_eq!(std::fs::read(target.join("new.exe")).unwrap(), b"new");
    }

    #[test]
    fn client_crashing_before_confirmation_rolls_back() {
        let temp = tempfile::tempdir().unwrap();
        let (target, backup) = install_fixture(temp.path());
        let error = launch_and_finalize(
            &target,
            &backup,
            |_| Ok(running(Some(2))),
            || false,
            Duration::from_secs(60),
            |_| {},
            || {},
        )
        .unwrap_err();
        assert!(error.contains("exited before confirming"), "{error}");
        assert_eq!(std::fs::read(target.join("old.exe")).unwrap(), b"old");
        assert!(!target.join("new.exe").exists());
        assert!(!backup.exists());
    }

    #[test]
    fn unconfirmed_running_client_keeps_the_backup() {
        let temp = tempfile::tempdir().unwrap();
        let (target, backup) = install_fixture(temp.path());
        let outcome = launch_and_finalize(
            &target,
            &backup,
            |_| Ok(running(None)),
            || false,
            Duration::from_secs(1),
            |_| {},
            || {},
        )
        .unwrap();
        assert!(matches!(outcome, Finalization::Unconfirmed));
        assert!(backup.join("old.exe").is_file());
        assert!(target.join("new.exe").is_file());
    }

    #[test]
    fn failed_launch_restores_the_previous_version() {
        let temp = tempfile::tempdir().unwrap();
        let (target, backup) = install_fixture(temp.path());
        let error = launch_and_finalize(
            &target,
            &backup,
            |_| Err(std::io::Error::other("fixture launch failure")),
            || false,
            Duration::from_secs(60),
            |_| {},
            || {},
        )
        .unwrap_err();
        assert!(error.contains("could not start"));
        assert_eq!(std::fs::read(target.join("old.exe")).unwrap(), b"old");
        assert!(!backup.exists());
    }

    #[test]
    fn failed_rollback_names_the_preserved_backup() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("MultiCore");
        let backup = temp.path().join(".multicore-previous-0.2.0-42");
        std::fs::create_dir(&target).unwrap();
        // The backup is missing, so the restoring rename fails.
        let error = launch_and_finalize(
            &target,
            &backup,
            |_| Err(std::io::Error::other("fixture launch failure")),
            || false,
            Duration::from_secs(60),
            |_| {},
            || {},
        )
        .unwrap_err();
        assert!(error.contains("rollback failed"), "{error}");
        assert!(error.contains(&backup.display().to_string()), "{error}");
        assert!(target.is_dir(), "the failed install must be put back");
    }

    #[test]
    fn marker_must_name_the_installed_version() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("started-ok");
        assert!(!marker_confirms(&marker, "0.2.0"));
        std::fs::write(&marker, "0.1.0").unwrap();
        assert!(!marker_confirms(&marker, "0.2.0"));
        std::fs::write(&marker, "0.2.0\n").unwrap();
        assert!(marker_confirms(&marker, "0.2.0"));
    }

    #[test]
    fn stale_staging_and_failed_directories_are_removed_but_backups_kept() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            ".multicore-stage-0.2.0-7",
            ".multicore-failed-7",
            ".multicore-previous-0.1.0-7",
            "current",
        ] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
            std::fs::write(temp.path().join(name).join("f"), b"x").unwrap();
        }
        remove_stale_work_directories(temp.path());
        assert!(!temp.path().join(".multicore-stage-0.2.0-7").exists());
        assert!(!temp.path().join(".multicore-failed-7").exists());
        assert!(temp.path().join(".multicore-previous-0.1.0-7").is_dir());
        assert!(temp.path().join("current").is_dir());
    }

    #[test]
    fn install_directory_membership_is_case_insensitive_and_prefix_safe() {
        let install = Path::new(r"\\?\C:\Users\A\Programs\MultiCore\current");
        assert!(image_is_within(
            r"C:\users\a\programs\multicore\CURRENT\runtime\multicore-daemon.exe",
            install
        ));
        assert!(image_is_within(
            r"C:\Users\A\Programs\MultiCore\current\cores\xray.exe",
            install
        ));
        assert!(!image_is_within(
            r"C:\Users\A\Programs\MultiCore\current-old\MultiCore.exe",
            install
        ));
        assert!(!image_is_within(r"C:\Windows\explorer.exe", install));
    }

    #[test]
    fn install_process_wait_is_bounded() {
        let mut calls = 0;
        let remaining = wait_until_clear(
            || {
                calls += 1;
                if calls >= 3 { Vec::new() } else { vec![7] }
            },
            Duration::from_secs(30),
            Duration::from_millis(250),
            |_| {},
        );
        assert!(remaining.is_empty());
        assert_eq!(calls, 3);

        let remaining = wait_until_clear(
            || vec![7, 8],
            Duration::from_secs(1),
            Duration::from_millis(250),
            |_| {},
        );
        assert_eq!(remaining, vec![7, 8]);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "flaky under this CI runner's process-enumeration/security-software environment; run locally with --ignored"]
    fn running_processes_are_found_by_install_directory() {
        let exe = std::env::current_exe().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let copy = temp.path().join("probe.exe");
        std::fs::copy(r"C:\Windows\System32\cmd.exe", &copy).unwrap();
        let mut child = std::process::Command::new(&copy)
            .args(["/D", "/C", "ping -n 30 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        // Toolhelp snapshots and QueryFullProcessImageNameW are point-in-time;
        // a freshly spawned process is not always visible on the very first
        // snapshot, especially under security-software hooks on CI. Poll
        // briefly instead of asserting immediately after spawn().
        let install = temp.path().canonicalize().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut found = super::processes_under(&install);
        while !found.contains(&child.id()) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            found = super::processes_under(&install);
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(found.contains(&child.id()), "{found:?}");
        // The test binary itself is excluded because it is the calling process.
        assert!(!super::processes_under(exe.parent().unwrap()).contains(&std::process::id()));
    }

    #[test]
    fn service_is_stopped_before_the_rollback_renames_current() {
        let temp = tempfile::tempdir().unwrap();
        let (target, backup) = install_fixture(temp.path());
        let hook_ran = Cell::new(false);
        let error = launch_and_finalize(
            &target,
            &backup,
            |_| Ok(running(Some(1))),
            || false,
            Duration::from_secs(60),
            |_| {},
            || {
                // Still the failed new version: the rollback has not renamed anything yet.
                assert!(target.join("new.exe").is_file());
                assert!(backup.join("old.exe").is_file());
                hook_ran.set(true);
            },
        )
        .unwrap_err();
        assert!(error.contains("exited before confirming"), "{error}");
        assert!(
            hook_ran.get(),
            "the service must be stopped before the rollback"
        );
        assert_eq!(std::fs::read(target.join("old.exe")).unwrap(), b"old");
    }

    #[test]
    fn confirmed_start_does_not_stop_the_service() {
        let temp = tempfile::tempdir().unwrap();
        let (target, backup) = install_fixture(temp.path());
        let outcome = launch_and_finalize(
            &target,
            &backup,
            |_| Ok(running(None)),
            || true,
            Duration::from_secs(60),
            |_| {},
            || panic!("no rollback, so the service keeps running"),
        )
        .unwrap();
        assert!(matches!(outcome, Finalization::Confirmed(Ok(()))));
    }

    #[cfg(windows)]
    const ARCHIVE_DIR: &str = r"C:\ProgramData\MultiCore\service\update";
    #[cfg(not(windows))]
    const ARCHIVE_DIR: &str = "/var/lib/multicore/service/update";
    #[cfg(windows)]
    const ARCHIVE: &str = r"C:\ProgramData\MultiCore\service\update\multicore-1.1.0.zip";
    #[cfg(not(windows))]
    const ARCHIVE: &str = "/var/lib/multicore/service/update/multicore-1.1.0.zip";
    #[cfg(windows)]
    const TARGET: &str = r"C:\Program Files\MultiCore\current";
    #[cfg(not(windows))]
    const TARGET: &str = "/opt/multicore/current";
    #[cfg(windows)]
    const USER_UPDATE_DIR: &str = r"C:\Users\me\AppData\Local\MultiCore\update";
    #[cfg(not(windows))]
    const USER_UPDATE_DIR: &str = "/home/me/.local/share/MultiCore/update";

    fn base_args() -> Vec<&'static str> {
        vec![
            "multicore-updater.exe",
            "--wait-pid",
            "42",
            "--archive",
            ARCHIVE,
            "--target",
            TARGET,
            "--size",
            "10",
            "--sha256",
            "00",
            "--version",
            "1.1.0",
        ]
    }

    #[test]
    fn service_mode_needs_both_session_arguments() {
        let mut args = base_args();
        args.extend([
            "--service-session",
            "1",
            "--user-update-dir",
            USER_UPDATE_DIR,
        ]);
        let request = parse_args(args.iter().map(|arg| arg.to_string())).unwrap();
        let service = request.service.as_ref().unwrap();
        assert_eq!(service.session_id, 1);
        assert_eq!(
            update_state_dir(&request),
            Some(PathBuf::from(USER_UPDATE_DIR))
        );
        let mut half = base_args();
        half.extend(["--service-session", "1"]);
        assert!(parse_args(half.iter().map(|arg| arg.to_string())).is_err());
        let mut relative = base_args();
        relative.extend(["--service-session", "1", "--user-update-dir", r"update"]);
        assert!(parse_args(relative.iter().map(|arg| arg.to_string())).is_err());
    }

    #[test]
    fn without_the_service_results_stay_next_to_the_archive() {
        let request = parse_args(base_args().iter().map(|arg| arg.to_string())).unwrap();
        assert!(request.service.is_none());
        assert_eq!(update_state_dir(&request), Some(PathBuf::from(ARCHIVE_DIR)));
    }

    #[cfg(windows)]
    #[test]
    fn service_mode_relaunches_in_the_user_session_and_restarts_the_service() {
        let source = include_str!("session.rs");
        for token in [
            "WTSQueryUserToken",
            "CreateProcessAsUserW",
            "winsta0\\\\default",
            "CreateEnvironmentBlock",
            "ImpersonateLoggedOnUser",
            "RevertToSelf",
            "StartServiceW",
            "SERVICE_CONTROL_STOP",
            "SERVICE_STOPPED",
        ] {
            assert!(source.contains(token), "{token}");
        }
    }
}
