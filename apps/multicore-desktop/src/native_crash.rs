//! Native crashes on Windows leave a trace in `logs/crash.log`.
//!
//! A Rust panic is written by the panic hook (`crash_log`), but an access violation in a
//! driver or in Skia, a stack overflow (Rust's own handler only prints «has overflowed its
//! stack» to a stderr a GUI app does not have) or any other unhandled SEH exception used to
//! end the process with nothing in the logs. [`install`] adds an unhandled-exception filter
//! that appends one report per crash:
//!
//! ```text
//! === native crash 1791052770210 · MULTICORE 1.1.8 (windows, x86_64) ===
//! exception 0xc0000005 (access violation, writing 0x8)
//! at multicore-desktop.exe+0x1a2b3c (0x7ff6...)
//! thread 1234 (main)
//! frames (module+offset):
//!   multicore-desktop.exe+0x...
//! ```
//!
//! The filter runs on the crashing thread, possibly on the last pages of an overflowed stack
//! or with the heap corrupted, so it allocates nothing: it formats into a static buffer and
//! appends it to crash.log through the path prepared at start-up. The main thread reserves
//! [`STACK_GUARANTEE`] for it (`SetThreadStackGuarantee`; Rust's other threads keep the
//! runtime's 20 KiB). Afterwards Windows Error Reporting handles the crash as before, so the
//! «Application Error» event and any dump still appear; the log bundle copies those events
//! (`log_bundle::windows_error_events`).
//!
//! The process's stderr (when it has none, as a GUI app started from Explorer) goes to
//! `logs/stderr.log`: the runtime's own last words before an abort (a panic that cannot
//! unwind, a failed allocation, a stack overflow) and every `eprintln!` of the app. The
//! process that deletes the data directory («Удалить все данные») keeps no file open in it.

use std::cell::UnsafeCell;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::FromRawHandle as _;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::{HMODULE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, LPTOP_LEVEL_EXCEPTION_FILTER, RtlCaptureStackBackTrace,
    SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, SetThreadStackGuarantee, Sleep};

/// Stack the main thread keeps for the filter after a stack overflow.
const STACK_GUARANTEE: u32 = 64 * 1024;
/// Frames written per report.
const MAX_FRAMES: usize = 48;
/// One report, frames included, fits.
const REPORT_BYTES: usize = 8 * 1024;
const STDERR_LOG_FILE_NAME: &str = "stderr.log";
/// At start-up a larger stderr.log keeps only its newest half.
const MAX_STDERR_LOG_BYTES: u64 = 512 * 1024;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

/// crash.log's path, NUL-terminated, for `CreateFileW` in the filter.
static CRASH_LOG_PATH: OnceLock<Vec<u16>> = OnceLock::new();
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
/// The filter that was installed before ours, called after the report is written.
static PREVIOUS_FILTER: AtomicUsize = AtomicUsize::new(0);
/// The thread writing a report right now (0: none).
static WRITER: AtomicU32 = AtomicU32::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

struct ReportBuffer {
    bytes: [u8; REPORT_BYTES],
    len: usize,
}

impl std::fmt::Write for ReportBuffer {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let room = REPORT_BYTES - self.len;
        let take = text.len().min(room);
        self.bytes[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

/// Static scratch space of the filter: the report and a module path.
struct Scratch {
    report: UnsafeCell<ReportBuffer>,
    module_path: UnsafeCell<[u16; 520]>,
}

// SAFETY: only the thread that won `WRITER` touches the cells (see `write_report`).
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch {
    report: UnsafeCell::new(ReportBuffer {
        bytes: [0; REPORT_BYTES],
        len: 0,
    }),
    module_path: UnsafeCell::new([0; 520]),
};

/// Installs the filter (once per process) writing to `crash_log`, reserves stack for it on
/// the calling (main) thread and sends stderr to `stderr.log` next to `crash_log` when the
/// process has no stderr of its own.
pub(crate) fn install(crash_log: &Path) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let resetting = std::env::args_os()
        .skip(1)
        .any(|argument| argument == crate::data_reset::RESET_FLAG);
    if let Some(logs) = crash_log.parent()
        && !resetting
    {
        let _ = std::fs::create_dir_all(logs);
        redirect_missing_stderr(&logs.join(STDERR_LOG_FILE_NAME));
    }
    let _ = CRASH_LOG_PATH.set(
        crash_log
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect(),
    );
    // SAFETY: no preconditions.
    MAIN_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    let mut guarantee = STACK_GUARANTEE;
    // SAFETY: the pointer is valid; the call only raises this thread's guarantee.
    unsafe { SetThreadStackGuarantee(&mut guarantee) };
    // SAFETY: `unhandled_exception` matches LPTOP_LEVEL_EXCEPTION_FILTER.
    let previous = unsafe { SetUnhandledExceptionFilter(Some(unhandled_exception)) };
    PREVIOUS_FILTER.store(
        previous.map_or(0, |filter| filter as usize),
        Ordering::SeqCst,
    );
}

/// A GUI process started from Explorer has no stderr: point it at `path` (appending, newest
/// half kept past [`MAX_STDERR_LOG_BYTES`]). A console or redirected stderr stays as it is.
fn redirect_missing_stderr(path: &Path) {
    // SAFETY: no preconditions.
    let current = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if !current.is_null() && current != INVALID_HANDLE_VALUE {
        return;
    }
    keep_newest_half(path, MAX_STDERR_LOG_BYTES);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = writeln!(
        file,
        "=== MULTICORE {} started {unix_ms} (pid {}) ===",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    use std::os::windows::io::IntoRawHandle as _;
    // The handle stays open for the life of the process as its stderr.
    let handle = file.into_raw_handle();
    // SAFETY: `handle` is a valid file handle owned by nobody else now.
    unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) };
}

/// Past `limit` bytes, `path` keeps only its newest half (from a line start).
fn keep_newest_half(path: &Path, limit: u64) {
    let _ = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let length = file.seek(SeekFrom::End(0))?;
        if length <= limit {
            return Ok(());
        }
        file.seek(SeekFrom::Start(length - limit / 2))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        let start = tail
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(tail.len(), |newline| newline + 1);
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&tail[start..])
    })();
}

unsafe extern "system" fn unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows passes valid exception pointers to the filter.
    unsafe { write_report(info) };
    let previous = PREVIOUS_FILTER.load(Ordering::SeqCst);
    if previous != 0 {
        // SAFETY: the value was a valid LPTOP_LEVEL_EXCEPTION_FILTER when stored.
        let previous: LPTOP_LEVEL_EXCEPTION_FILTER = unsafe { std::mem::transmute(previous) };
        if let Some(previous) = previous {
            // SAFETY: as above.
            return unsafe { previous(info) };
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}

/// Formats and appends the report for `info`; one thread at a time, a nested crash on the
/// writing thread itself is ignored.
unsafe fn write_report(info: *const EXCEPTION_POINTERS) {
    // SAFETY: no preconditions.
    let me = unsafe { GetCurrentThreadId() };
    let mut waited = 0;
    loop {
        match WRITER.compare_exchange(0, me, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(writer) if writer == me => return,
            Err(_) if waited < 200 => {
                waited += 1;
                // SAFETY: no preconditions.
                unsafe { Sleep(10) };
            }
            Err(_) => return,
        }
    }
    // SAFETY: this thread owns the scratch space until WRITER is released below.
    let (report, module_path) =
        unsafe { (&mut *SCRATCH.report.get(), &mut *SCRATCH.module_path.get()) };
    report.len = 0;
    // SAFETY: `info` and its record come from Windows (or the tests) and are valid.
    unsafe { format_report(report, module_path, info, me) };
    if let Some(path) = CRASH_LOG_PATH.get() {
        // SAFETY: `path` is NUL-terminated; the other arguments are plain flags.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                FILE_APPEND_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
            // SAFETY: a valid handle this function owns; dropping the File closes it.
            let mut file = unsafe { File::from_raw_handle(handle) };
            let _ = file.write_all(&report.bytes[..report.len]);
        }
    }
    WRITER.store(0, Ordering::Release);
}

unsafe fn format_report(
    out: &mut ReportBuffer,
    module_path: &mut [u16; 520],
    info: *const EXCEPTION_POINTERS,
    thread: u32,
) {
    let unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = writeln!(
        out,
        "=== native crash {unix_ms} · MULTICORE {} ({}, {}) ===",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    // SAFETY: the caller passes valid exception pointers.
    let record = unsafe { info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) };
    if let Some(record) = record {
        let code = record.ExceptionCode as u32;
        let _ = write!(out, "exception {code:#010x} ({}", exception_name(code));
        if code == 0xC000_0005 && record.NumberParameters >= 2 {
            let access = match record.ExceptionInformation[0] {
                0 => "reading",
                1 => "writing",
                8 => "executing",
                _ => "accessing",
            };
            let _ = write!(out, ", {access} {:#x}", record.ExceptionInformation[1]);
        }
        let _ = writeln!(out, ")");
        let _ = write!(out, "at ");
        write_location(out, module_path, record.ExceptionAddress as usize);
        let _ = writeln!(out);
    } else {
        let _ = writeln!(out, "exception record missing");
    }
    let role = if thread == MAIN_THREAD.load(Ordering::SeqCst) {
        " (main)"
    } else {
        ""
    };
    let _ = writeln!(out, "thread {thread}{role}");
    let mut frames = [std::ptr::null_mut(); MAX_FRAMES];
    // SAFETY: the buffer holds MAX_FRAMES pointers; a null hash pointer is allowed.
    let captured = unsafe {
        RtlCaptureStackBackTrace(
            0,
            MAX_FRAMES as u32,
            frames.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    } as usize;
    let _ = writeln!(out, "frames (module+offset):");
    for &frame in &frames[..captured.min(MAX_FRAMES)] {
        let _ = write!(out, "  ");
        write_location(out, module_path, frame as usize);
        let _ = writeln!(out);
    }
    let _ = writeln!(out);
}

/// `module+0xoffset (0xaddress)`, or the bare address outside every module.
fn write_location(out: &mut ReportBuffer, module_path: &mut [u16; 520], address: usize) {
    let mut module: HMODULE = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS takes any address; UNCHANGED_REFCOUNT leaves nothing to free.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    } != 0;
    if !found || module.is_null() {
        let _ = write!(out, "{address:#x}");
        return;
    }
    // SAFETY: the buffer length is passed; the module handle is valid.
    let length =
        unsafe { GetModuleFileNameW(module, module_path.as_mut_ptr(), module_path.len() as u32) }
            as usize;
    let path = &module_path[..length.min(module_path.len())];
    let name_start = path
        .iter()
        .rposition(|&unit| unit == u16::from(b'\\') || unit == u16::from(b'/'))
        .map_or(0, |separator| separator + 1);
    for character in char::decode_utf16(path[name_start..].iter().copied()) {
        let _ = out.write_char(character.unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    let _ = write!(
        out,
        "+{:#x} ({address:#x})",
        address.wrapping_sub(module as usize)
    );
}

fn exception_name(code: u32) -> &'static str {
    match code {
        0xC000_0005 => "access violation",
        0xC000_00FD => "stack overflow",
        0xC000_0409 => "stack buffer overrun / fail fast",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer divide by zero",
        0xC000_0095 => "integer overflow",
        0xC000_0374 => "heap corruption",
        0xC000_0006 => "in-page error",
        0xC000_0420 => "assertion failure",
        0xC000_0096 => "privileged instruction",
        0xC000_008C => "array bounds exceeded",
        0x8000_0003 => "breakpoint",
        0x4000_0015 => "fatal app exit",
        0xE06D_7363 => "C++ exception",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const CHILD_ENV: &str = "MULTICORE_NATIVE_CRASH_CHILD";
    const LOG_ENV: &str = "MULTICORE_NATIVE_CRASH_LOG";

    /// The crashing half of the tests below: runs only in a child process they start.
    #[test]
    #[ignore = "crashes on purpose; started by the native crash tests in a child process"]
    fn crash_child() {
        let Some(kind) = std::env::var_os(CHILD_ENV) else {
            return;
        };
        let log = std::path::PathBuf::from(std::env::var_os(LOG_ENV).expect("log path"));
        // Ends the child with the exception code once the report is written, without Windows
        // Error Reporting (no event, dialog or dump for a test). Installed first, it is the
        // filter ours chains to.
        unsafe extern "system" fn terminate_quietly(_: *const EXCEPTION_POINTERS) -> i32 {
            1 // EXCEPTION_EXECUTE_HANDLER
        }
        // SAFETY: the filter matches LPTOP_LEVEL_EXCEPTION_FILTER.
        unsafe { SetUnhandledExceptionFilter(Some(terminate_quietly)) };
        install(&log);
        match kind.to_str() {
            Some("access-violation") => {
                // SAFETY: none; this test crashes on purpose.
                unsafe { std::ptr::write_volatile(std::hint::black_box(8_usize) as *mut u32, 1) };
            }
            Some("stack-overflow") => {
                std::thread::Builder::new()
                    .name("multicore-overflow-test".into())
                    .stack_size(256 * 1024)
                    .spawn(|| recurse(std::hint::black_box(1)))
                    .expect("spawn")
                    .join()
                    .expect("join");
            }
            other => panic!("unknown crash kind {other:?}"),
        }
        unreachable!("the crash ends the process");
    }

    #[inline(never)]
    #[allow(unconditional_recursion)]
    fn recurse(depth: u64) -> u64 {
        let pad = std::hint::black_box([depth; 64]);
        recurse(depth + 1) + pad[(depth % 64) as usize]
    }

    fn crash_in_child(kind: &str) -> (Option<i32>, String) {
        let directory = tempfile::tempdir().expect("temp dir");
        let log = directory.path().join("logs").join("crash.log");
        let status = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "native_crash::tests::crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, kind)
            .env(LOG_ENV, &log)
            .output()
            .expect("run the crashing child");
        let report = std::fs::read_to_string(&log).unwrap_or_default();
        (status.status.code(), report)
    }

    #[test]
    fn an_access_violation_is_written_with_its_module_and_thread() {
        let (code, report) = crash_in_child("access-violation");
        assert_eq!(code.map(|code| code as u32), Some(0xC000_0005), "{report}");
        assert!(report.starts_with("=== native crash "), "{report}");
        assert!(
            report.contains("exception 0xc0000005 (access violation, writing 0x8)"),
            "{report}"
        );
        assert!(report.contains("at multicore_desktop-"), "{report}");
        assert!(report.contains(".exe+0x"), "{report}");
        assert!(report.contains("\nthread "), "{report}");
        assert!(report.contains("frames (module+offset):\n  "), "{report}");
    }

    #[test]
    fn a_stack_overflow_is_written_from_the_overflowed_thread() {
        let (code, report) = crash_in_child("stack-overflow");
        assert_eq!(code.map(|code| code as u32), Some(0xC000_00FD), "{report}");
        assert!(
            report.contains("exception 0xc00000fd (stack overflow)"),
            "{report}"
        );
        assert!(report.contains(".exe+0x"), "{report}");
        assert!(!report.contains("(main)"), "{report}");
    }

    #[test]
    fn a_large_stderr_log_keeps_its_newest_half() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join(STDERR_LOG_FILE_NAME);
        let line = "x".repeat(99) + "\n";
        std::fs::write(&path, line.repeat(20)).expect("write");
        keep_newest_half(&path, 4096);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            line.repeat(20)
        );
        std::fs::write(&path, format!("{}last\n", line.repeat(100))).expect("write");
        keep_newest_half(&path, 4096);
        let kept = std::fs::read_to_string(&path).expect("read");
        assert!(kept.len() <= 2048, "{}", kept.len());
        assert!(kept.ends_with("last\n"));
        assert!(kept.starts_with('x'));
    }

    #[test]
    fn exception_codes_have_names() {
        assert_eq!(exception_name(0xC000_0005), "access violation");
        assert_eq!(exception_name(0xC000_00FD), "stack overflow");
        assert_eq!(
            exception_name(0xC000_0409),
            "stack buffer overrun / fail fast"
        );
        assert_eq!(exception_name(0x1234), "unknown");
    }
}
