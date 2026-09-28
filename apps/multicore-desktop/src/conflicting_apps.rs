//! Detects other Mihomo/Xray-based VPN client apps that are known (or
//! suspected) to conflict with MultiCore's TUN adapter, WinTun driver
//! session, or local ports when both run at the same time.
//!
//! This is entirely local process-image-name matching: no network calls, no
//! elevation, and it never blocks a Connect attempt or kills anything. It is
//! purely informational input for a dismissible UI banner.

/// One conflicting app: the display name shown in the UI banner, and the set
/// of process image filename stems (no extension, matched case-insensitively
/// against the *whole* stem, never a substring) known or suspected to be
/// used by its installers/builds across platforms. Aliases are intentionally
/// permissive — multiple naming variants per app — because exact
/// casing/hyphenation is uncertain without being able to verify against the
/// real installers; missing a variant (false negative) is expected and fine
/// to tune later, but matching the full stem (not a loose substring) avoids
/// false positives against unrelated processes.
struct ConflictingApp {
    name: &'static str,
    aliases: &'static [&'static str],
}

const CONFLICTING_APPS: &[ConflictingApp] = &[
    ConflictingApp {
        // Confirmed live on real hardware: this project's own subscription
        // contract already references the "flclashx" vendor prefix
        // (docs/subscription-http-contract.md), and the actual installed app
        // in this ecosystem runs as FlClashX.exe / FlClashCore.exe (a
        // rebranded/white-label FlClash fork), not bare "FlClash" - its own
        // TUN adapter shows as "FlClashX" / "Meta Tunnel". Keep the plain
        // alias too in case an unbranded upstream build is ever encountered.
        //
        // Deliberately NOT flagging FlClashHelperService: confirmed live
        // (real Connect attempts, with and without it running) that this
        // background Windows service does not actually conflict - only the
        // GUI app's own TUN adapter does. Flagging it produced false-positive
        // banners telling users to close something that was never the
        // problem.
        name: "FlClash",
        aliases: &["flclash", "flclashx", "flclashcore"],
    },
    ConflictingApp {
        name: "Happ",
        aliases: &["happ", "happ_windows", "happ-desktop"],
    },
    ConflictingApp {
        name: "Incy",
        aliases: &["incy", "incy-windows-setup"],
    },
    ConflictingApp {
        name: "Prizrak-Box",
        aliases: &["prizrak-box", "prizrakbox"],
    },
    ConflictingApp {
        name: "Koala Clash",
        aliases: &["koalaclash", "koala-clash"],
    },
];

/// Normalizes a process image filename to a lowercase stem with any trailing
/// `.exe` removed, matching how Windows image filenames are compared.
fn stem_of(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    match lower.strip_suffix(".exe") {
        Some(stem) => stem.to_string(),
        None => lower,
    }
}

/// Pure matching logic shared by both platform enumerations below, and
/// exercised directly by unit tests independent of real process listing.
/// Returns detected display names, deduplicated and sorted.
fn detect_from_stems(stems: impl Iterator<Item = String>) -> Vec<&'static str> {
    let mut found = Vec::new();
    for raw in stems {
        let stem = stem_of(&raw);
        for app in CONFLICTING_APPS {
            if !found.contains(&app.name) && app.aliases.iter().any(|alias| *alias == stem) {
                found.push(app.name);
            }
        }
    }
    found.sort_unstable();
    found
}

/// Scans currently running processes for known conflicting VPN client apps.
/// Cheap and non-blocking: a single process snapshot/`/proc` walk. Returns
/// detected display names, deduplicated and sorted; an empty result means no
/// known conflict was found (including when enumeration itself fails, which
/// is treated as "nothing detected" since this is best-effort and must never
/// surface as an error to the user).
pub fn scan_running() -> Vec<&'static str> {
    detect_from_stems(running_process_image_names())
}

/// Terminates every currently running process whose image stem matches one
/// of `app_name`'s known aliases (an unrecognized name matches nothing).
/// Best-effort and never elevated: a process this user's own token cannot
/// open or terminate (a different user, or one already exiting) is silently
/// skipped rather than surfaced as an error, matching `scan_running`'s
/// "never block, never surprise the user with a hard failure" contract.
/// Returns how many processes were successfully signaled to terminate.
pub fn close_running(app_name: &str) -> usize {
    let Some(app) = CONFLICTING_APPS.iter().find(|app| app.name == app_name) else {
        return 0;
    };
    let mut closed = 0;
    for (pid, image) in running_process_entries() {
        if app.aliases.iter().any(|alias| *alias == stem_of(&image)) && terminate_pid(pid) {
            closed += 1;
        }
    }
    closed
}

fn running_process_image_names() -> impl Iterator<Item = String> {
    running_process_entries().map(|(_pid, name)| name)
}

#[cfg(windows)]
fn running_process_entries() -> impl Iterator<Item = (u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut entries = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot != INVALID_HANDLE_VALUE {
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            if len > 0 {
                entries.push((
                    entry.th32ProcessID,
                    String::from_utf16_lossy(&entry.szExeFile[..len]),
                ));
            }
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };
    }
    entries.into_iter()
}

#[cfg(target_os = "linux")]
fn running_process_entries() -> impl Iterator<Item = (u32, String)> {
    let mut entries = Vec::new();
    if let Ok(dir_entries) = std::fs::read_dir("/proc") {
        for entry in dir_entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            // `comm` is a short (<=15 byte), always-available, non-root-only
            // process name; every alias above fits well within that limit.
            if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
                let name = comm.trim();
                if !name.is_empty() {
                    entries.push((pid, name.to_string()));
                }
            }
        }
    }
    entries.into_iter()
}

#[cfg(not(any(windows, target_os = "linux")))]
fn running_process_entries() -> impl Iterator<Item = (u32, String)> {
    std::iter::empty()
}

/// Best-effort process termination with this user's own privileges only:
/// never elevates, and a process it cannot open or signal (wrong user,
/// already gone) is simply not counted as closed.
#[cfg(windows)]
fn terminate_pid(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let ok = unsafe { TerminateProcess(handle, 1) } != 0;
    unsafe { CloseHandle(handle) };
    ok
}

#[cfg(target_os = "linux")]
fn terminate_pid(pid: u32) -> bool {
    // No new process-signaling dependency: `kill` is present on every Linux
    // system this app already targets (see scripts/test-linux-shell.sh and
    // friends running against real distros), and this stays a plain,
    // unprivileged SIGTERM - never sudo/pkexec, matching this feature's
    // "only what the user's own token can already do" contract.
    std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(any(windows, target_os = "linux")))]
fn terminate_pid(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stems(values: &[&str]) -> impl Iterator<Item = String> {
        values
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn detects_nothing_when_no_processes_match() {
        let found = detect_from_stems(stems(&[
            "multicore-desktop.exe",
            "multicore-daemon.exe",
            "explorer.exe",
            "chrome.exe",
        ]));
        assert!(found.is_empty());
    }

    #[test]
    fn matches_case_insensitively() {
        let found = detect_from_stems(stems(&["FlClash.EXE"]));
        assert_eq!(found, vec!["FlClash"]);
    }

    #[test]
    fn matches_the_real_flclashx_process_family() {
        // Confirmed on real hardware: the actually-distributed app in this
        // ecosystem runs as FlClashX.exe with a separate core process, not
        // bare FlClash.exe.
        for exe in ["FlClashX.exe", "FlClashCore.exe"] {
            assert_eq!(detect_from_stems(stems(&[exe])), vec!["FlClash"], "{exe}");
        }
    }

    #[test]
    fn does_not_flag_the_flclash_helper_service() {
        // Confirmed live (real Connect attempts, with and without it
        // running): this background Windows service does not actually
        // conflict - only the GUI app's own TUN adapter does. Flagging it
        // produced false-positive banners over nothing.
        assert!(detect_from_stems(stems(&["FlClashHelperService.exe"])).is_empty());
    }

    #[test]
    fn matches_multiple_alias_variants_for_the_same_app() {
        assert_eq!(detect_from_stems(stems(&["happ.exe"])), vec!["Happ"]);
        assert_eq!(
            detect_from_stems(stems(&["happ_windows.exe"])),
            vec!["Happ"]
        );
        assert_eq!(detect_from_stems(stems(&["happ-desktop"])), vec!["Happ"]);
    }

    #[test]
    fn detects_multiple_simultaneous_conflicts_deduplicated_and_sorted() {
        let found = detect_from_stems(stems(&[
            "Incy.exe",
            "incy.exe",
            "KoalaClash.exe",
            "explorer.exe",
        ]));
        assert_eq!(found, vec!["Incy", "Koala Clash"]);
    }

    #[test]
    fn does_not_substring_match_similarly_named_processes() {
        // "happiness.exe" contains "happ" as a substring but must not match:
        // matching is on the whole stem only.
        let found = detect_from_stems(stems(&[
            "happiness.exe",
            "flclashy_notes.exe",
            "incyclopedia.exe",
            "prizrak-box-uninstall-helper.exe",
        ]));
        assert!(found.is_empty(), "unexpected match: {found:?}");
    }

    #[test]
    fn does_not_match_this_app_own_processes() {
        let found = detect_from_stems(stems(&[
            "MultiCore.exe",
            "multicore-daemon.exe",
            "multicore-core-host.exe",
            "multicore-updater.exe",
        ]));
        assert!(found.is_empty());
    }

    #[test]
    fn matches_prizrak_box_and_koala_clash_hyphenated_and_camelcase_variants() {
        assert_eq!(
            detect_from_stems(stems(&["Prizrak-Box.exe"])),
            vec!["Prizrak-Box"]
        );
        assert_eq!(
            detect_from_stems(stems(&["PrizrakBox.exe"])),
            vec!["Prizrak-Box"]
        );
        assert_eq!(
            detect_from_stems(stems(&["koala-clash"])),
            vec!["Koala Clash"]
        );
    }

    #[test]
    fn scan_running_never_panics_and_returns_a_plain_vec() {
        // Smoke test against the real platform enumeration: must complete
        // without panicking regardless of what happens to be running.
        let _ = scan_running();
    }

    #[test]
    fn close_running_ignores_an_unrecognized_app_name_without_enumerating() {
        assert_eq!(close_running("Not A Real App"), 0);
        assert_eq!(close_running(""), 0);
    }
}
