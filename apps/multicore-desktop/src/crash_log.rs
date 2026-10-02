//! A panic of the desktop app used to close it without a trace: the window vanished, Windows
//! recorded no application error (a Rust panic exits normally) and the daemon log only showed
//! the client going away. The hook installed by [`install`] writes every panic (message,
//! thread, location, backtrace) to `logs/crash.log` in the data directory, which «Сохранить
//! лог…» puts in its archive like every file under `logs/`, and adds a line to
//! `client-errors.log`; then the default hook prints it to stderr as before.
//!
//! Release builds carry no symbols, so on Windows each frame is also written as
//! `module+offset`, which the build's PDB resolves afterwards.

use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub(crate) const CRASH_LOG_FILE_NAME: &str = "crash.log";
/// Past this size the file keeps only its newest half (a few reports).
const MAX_CRASH_LOG_BYTES: u64 = 256 * 1024;
/// Raw frames written per report.
const MAX_RAW_FRAMES: usize = 96;

/// Installs the hook; the previous hook (the default one: stderr) still runs after it.
pub(crate) fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let report = panic_report(info, env!("CARGO_PKG_VERSION"));
        if let Some(path) = crash_log_path() {
            append_report(&path, &report);
        }
        crate::daemon::record_low_level("panic", first_lines(&report, 3));
        previous(info);
    }));
}

#[cfg(not(test))]
fn crash_log_path() -> Option<PathBuf> {
    multicore_platform::dirs::user_data_root()
        .ok()
        .map(|root| root.join("logs").join(CRASH_LOG_FILE_NAME))
}

/// Tests never write into the real user's data directory.
#[cfg(test)]
fn crash_log_path() -> Option<PathBuf> {
    Some(
        std::env::temp_dir()
            .join(format!("multicore-desktop-test-{}", std::process::id()))
            .join("logs")
            .join(CRASH_LOG_FILE_NAME),
    )
}

/// One report: when, which version and thread, where and why, then the backtrace.
pub(crate) fn panic_report(info: &std::panic::PanicHookInfo<'_>, version: &str) -> String {
    let thread = std::thread::current();
    let thread = thread.name().unwrap_or("<unnamed>");
    let message = info
        .payload_as_str()
        .unwrap_or("<panic payload is not a string>");
    let location = info
        .location()
        .map(|location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        })
        .unwrap_or_else(|| "<unknown location>".into());
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut report = format!(
        "=== panic {unix_ms} · MULTICORE {version} ({}, {}) ===\nthread '{thread}' panicked at {location}:\n{message}\n\nbacktrace:\n{}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::backtrace::Backtrace::force_capture(),
    );
    let raw = raw_frames();
    if !raw.is_empty() {
        report.push_str("\nframes (module+offset):\n");
        for frame in raw {
            let _ = writeln!(report, "  {frame}");
        }
    }
    report.push('\n');
    report
}

fn first_lines(text: &str, count: usize) -> String {
    text.lines().take(count).collect::<Vec<_>>().join(" | ")
}

/// Appends `report`; a file past [`MAX_CRASH_LOG_BYTES`] first keeps only its newest half.
/// Every failure is ignored: the app is already going down.
pub(crate) fn append_report(path: &Path, report: &str) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let length = file.seek(SeekFrom::End(0))?;
        if length > MAX_CRASH_LOG_BYTES {
            let mut contents = Vec::new();
            file.seek(SeekFrom::Start(0))?;
            file.read_to_end(&mut contents)?;
            let cut = contents
                .len()
                .saturating_sub((MAX_CRASH_LOG_BYTES / 2) as usize);
            let kept = contents[cut..].to_vec();
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(&kept)?;
        }
        file.write_all(report.as_bytes())?;
        file.flush()
    })();
}

/// The calling thread's return addresses as `module+0xoffset`.
#[cfg(windows)]
fn raw_frames() -> Vec<String> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };

    let mut frames = [std::ptr::null_mut(); MAX_RAW_FRAMES];
    // SAFETY: the buffer holds MAX_RAW_FRAMES pointers; a null hash pointer is allowed.
    let captured = unsafe {
        RtlCaptureStackBackTrace(
            0,
            MAX_RAW_FRAMES as u32,
            frames.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    } as usize;
    frames[..captured.min(MAX_RAW_FRAMES)]
        .iter()
        .map(|&address| {
            let mut module: HMODULE = std::ptr::null_mut();
            // SAFETY: FROM_ADDRESS takes any address; UNCHANGED_REFCOUNT leaves nothing to free.
            let found = unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    address as *const u16,
                    &mut module,
                )
            } != 0;
            if !found || module.is_null() {
                return format!("{address:p}");
            }
            let mut name = [0_u16; 260];
            // SAFETY: the buffer length is passed; the module handle is valid.
            let length = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) }
                as usize;
            let path = PathBuf::from(std::ffi::OsString::from_wide(
                &name[..length.min(name.len())],
            ));
            let module_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".into());
            format!(
                "{module_name}+{:#x}",
                (address as usize).wrapping_sub(module as usize)
            )
        })
        .collect()
}

#[cfg(not(windows))]
fn raw_frames() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// The panic hook is process-wide: the tests that replace it take turns.
    static HOOK: Mutex<()> = Mutex::new(());

    /// Runs `f` under a hook that keeps the report it builds (the process-wide hook is shared
    /// by every test thread, so only this thread's panic is kept).
    fn report_of(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let _hook = HOOK.lock().unwrap_or_else(|e| e.into_inner());
        let kept = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&kept);
        let me = std::thread::current().id();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() == me {
                *sink.lock().unwrap_or_else(|e| e.into_inner()) = panic_report(info, "9.9.9");
            }
        }));
        let result = std::panic::catch_unwind(f);
        std::panic::set_hook(previous);
        assert!(result.is_err(), "the closure panicked");
        kept.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[test]
    fn a_panic_report_names_the_message_thread_location_and_frames() {
        let report = std::thread::Builder::new()
            .name("multicore-crash-test".into())
            .spawn(|| report_of(|| panic!("Recursion detected")))
            .expect("spawn")
            .join()
            .expect("join");
        assert!(report.contains("MULTICORE 9.9.9"), "{report}");
        assert!(
            report.contains("thread 'multicore-crash-test' panicked at"),
            "{report}"
        );
        assert!(report.contains("crash_log.rs:"), "{report}");
        assert!(report.contains("\nRecursion detected\n"), "{report}");
        assert!(report.contains("backtrace:"), "{report}");
        if cfg!(windows) {
            assert!(report.contains("frames (module+offset):"), "{report}");
            assert!(report.contains(".exe+0x"), "{report}");
        }
    }

    #[test]
    fn reports_are_appended_and_the_file_stays_bounded() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("logs").join(CRASH_LOG_FILE_NAME);
        append_report(&path, "=== first ===\n");
        append_report(&path, "=== second ===\n");
        let text = std::fs::read_to_string(&path).expect("crash log");
        assert_eq!(text, "=== first ===\n=== second ===\n");

        let big = "x".repeat(64 * 1024);
        for _ in 0..8 {
            append_report(&path, &big);
        }
        append_report(&path, "=== newest ===\n");
        let size = std::fs::metadata(&path).expect("metadata").len();
        assert!(size <= MAX_CRASH_LOG_BYTES + 64 * 1024 + 64, "{size}");
        let text = std::fs::read_to_string(&path).expect("crash log");
        assert!(text.ends_with("=== newest ===\n"));
        assert!(!text.contains("=== first ==="));
    }

    #[test]
    fn the_installed_hook_writes_the_crash_log_and_a_client_error_line() {
        // The hook installed by the app (paths redirected to a per-process test directory).
        let _hook = HOOK.lock().unwrap_or_else(|e| e.into_inner());
        let path = crash_log_path().expect("test crash log path");
        let _ = std::fs::remove_file(&path);
        let previous = std::panic::take_hook();
        install();
        let result = std::thread::Builder::new()
            .name("multicore-crash-install-test".into())
            .spawn(|| panic!("installed hook test panic"))
            .expect("spawn")
            .join();
        let ours = std::panic::take_hook();
        std::panic::set_hook(previous);
        drop(ours);
        assert!(result.is_err());
        let text = std::fs::read_to_string(&path).expect("crash log written");
        assert!(text.contains("installed hook test panic"), "{text}");
        assert!(text.contains("multicore-crash-install-test"), "{text}");
        let client_errors = path.with_file_name("client-errors.log");
        let line = std::fs::read_to_string(&client_errors).expect("client-errors.log written");
        assert!(
            line.lines()
                .any(|line| line.contains("	panic	") && line.contains("installed hook test panic")),
            "{line}"
        );
    }
}
