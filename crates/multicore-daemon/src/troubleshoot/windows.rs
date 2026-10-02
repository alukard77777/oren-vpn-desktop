//! Windows detection, the user-level fixes, and the UAC-elevated helper launch.
//!
//! Everything here runs as the logged-in user. Detection only reads; the user-level fixes are
//! the stale/foreign WinINet proxy (switched off), the DNS cache (`ipconfig /flushdns`) and
//! Drover's files in the user's own Discord folders.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::{null, null_mut},
    sync::Arc,
    time::{Duration, Instant},
};

use multicore_core::troubleshoot::{
    CheckStep, DpiService, FixId, FixOutcome, FixReport, ListenPort, PrivilegedFix, StepReport,
    decode_helper_exit, helper_arguments,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, GetLastError, GlobalFree,
        WAIT_OBJECT_0,
    },
    NetworkManagement::{
        IpHelper::{
            GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
            MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
            UDP_TABLE_OWNER_PID,
        },
        Rras::{RASCONNW, RasEnumConnectionsW},
    },
    Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_PROXY_INFO, WinHttpGetDefaultProxyConfiguration,
    },
    System::{
        Com::CoTaskMemFree,
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Registry::{
            HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_64KEY, REG_DWORD, RegCloseKey,
            RegOpenKeyExW, RegQueryValueExW,
        },
        Services::{
            CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW,
            OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_ENUM_PROCESS_INFO, SC_HANDLE,
            SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACTIVE, SERVICE_QUERY_STATUS,
            SERVICE_STATUS, SERVICE_STOPPED, SERVICE_WIN32,
        },
        SystemInformation::GetSystemDirectoryW,
        Threading::{
            CREATE_NO_WINDOW, GetExitCodeProcess, OpenProcess, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, WaitForSingleObject,
        },
    },
    UI::{
        Shell::{
            FOLDERID_Desktop, FOLDERID_Downloads, FOLDERID_LocalAppData, KF_FLAG_DEFAULT,
            SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, SHGetKnownFolderPath,
            ShellExecuteExW,
        },
        WindowsAndMessaging::SW_HIDE,
    },
};

use super::{
    DpiServiceState, DroverRemoval, DroverState, ElevatedFixes, HelperError, NETWORK_POLICY_VALUES,
    PortOwner, ProxyOutcome, ServiceFixes, Troubleshooting, antivirus_findings, classify_antivirus,
    clock, clock_report, clock_state, dpi_report, drover_report, hosts, hosts_findings,
    looks_like_our_proxy, network_report, policy_findings, port_findings, proxy_findings,
    run_fixes, vpn_findings,
};
use crate::system_proxy::{InternetSettings, WinInetSettings};

/// How long the user has to answer the administrator prompt and the fixes have to run.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// `ERROR_BUFFER_TOO_SMALL` of the RAS API (raserror.h).
const ERROR_BUFFER_TOO_SMALL: u32 = 603;
const TOOL_TIMEOUT: Duration = Duration::from_secs(20);
const DISCORD_FOLDERS: [&str; 3] = ["Discord", "DiscordCanary", "DiscordPTB"];
const DISCORD_IMAGES: [&str; 3] = ["discord.exe", "discordcanary.exe", "discordptb.exe"];
const DROVER_FILES: [&str; 2] = ["version.dll", "drover.ini"];
/// `%USERPROFILE%\Downloads|Desktop\<name>` folders the owner's cleanup tool warns about.
const DPI_FOLDERS: [&str; 4] = ["zapret", "goodbyedpi", "discordfix", "discord fix"];

/// The production troubleshooting of the Windows daemon.
#[derive(Clone)]
pub struct SystemTroubleshooting {
    /// The always-ready service, used for privileged fixes when it is there.
    pub service: Option<Arc<dyn ServiceFixes>>,
    /// The daemon's system-proxy marker (`system_proxy`): MULTICORE's own leftover proxy.
    pub proxy_marker: PathBuf,
    /// `<package>`: listeners inside it are MULTICORE's own.
    pub package_root: Option<PathBuf>,
    /// Ports the daemon itself needs besides the subscription's (its Mihomo controller).
    pub extra_ports: Vec<ListenPort>,
}

#[async_trait::async_trait]
impl Troubleshooting for SystemTroubleshooting {
    async fn check(&self, step: CheckStep, ports: Vec<ListenPort>) -> StepReport {
        if step == CheckStep::Time {
            return clock_report(clock_state(clock::measure_skew().await), true);
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.check_blocking(step, ports))
            .await
            .unwrap_or_default()
    }

    async fn fix(&self, fixes: Vec<FixId>) -> FixReport {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            run_fixes(&fixes, &remove_drover, this.service.as_deref(), &UacHelper)
        })
        .await
        .unwrap_or_default()
    }
}

impl SystemTroubleshooting {
    fn check_blocking(&self, step: CheckStep, ports: Vec<ListenPort>) -> StepReport {
        let findings = |findings| StepReport {
            findings,
            fixes: Vec::new(),
        };
        match step {
            CheckStep::Antivirus => findings(antivirus_findings(&classify_antivirus(
                active_services().iter().map(String::as_str),
            ))),
            CheckStep::Dpi => dpi_report(&dpi_services(), &dpi_folders()),
            CheckStep::Ports => {
                let mut wanted = ports;
                wanted.extend(self.extra_ports.iter().copied());
                let root = self
                    .package_root
                    .as_ref()
                    .map(|root| root.to_string_lossy().into_owned());
                findings(port_findings(&wanted, &listeners(&wanted), root.as_deref()))
            }
            CheckStep::Proxy => {
                let (outcome, pac_host) = clear_system_proxy(&self.proxy_marker);
                let pac_host = pac_host.as_ref().map(|host| {
                    host.as_deref()
                        .map_or(super::PacHost::Unrecognized, super::PacHost::Named)
                });
                findings(proxy_findings(&outcome, pac_host))
            }
            CheckStep::Vpn => findings(vpn_findings(&ras_connections())),
            CheckStep::Discord => drover_report(scan_drover().as_ref()),
            CheckStep::Hosts => {
                let path = system_directory()
                    .map(|system| system.join("drivers").join("etc").join("hosts"))
                    .unwrap_or_default();
                let machine = std::env::var("COMPUTERNAME").ok();
                let entries = hosts::read_hosts(&path)
                    .map(|contents| hosts::custom_entries(&contents, machine.as_deref()))
                    .unwrap_or_default();
                findings(hosts_findings(&path.to_string_lossy(), &entries))
            }
            CheckStep::Policies => findings(policy_findings(&restrictive_policies())),
            CheckStep::Network => network_report(Some(flush_dns()), winhttp_proxy().as_deref()),
            CheckStep::Time => StepReport::default(),
        }
    }
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn from_wide_nul(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

fn system_directory() -> Option<PathBuf> {
    let mut buffer = vec![0_u16; 512];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    (length > 0 && length < buffer.len())
        .then(|| PathBuf::from(OsString::from_wide(&buffer[..length])))
}

fn known_folder(folder: &windows_sys::core::GUID) -> Option<PathBuf> {
    let mut raw = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT as u32, null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        if !raw.is_null() {
            unsafe { CoTaskMemFree(raw.cast()) };
        }
        return None;
    }
    let mut length = 0;
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, length)
    }));
    unsafe { CoTaskMemFree(raw.cast()) };
    Some(path)
}

struct ScHandle(SC_HANDLE);
impl Drop for ScHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// Names of the running Win32 services.
fn active_services() -> Vec<String> {
    let manager = unsafe {
        OpenSCManagerW(
            null(),
            null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )
    };
    if manager.is_null() {
        return Vec::new();
    }
    let manager = ScHandle(manager);
    let mut names = Vec::new();
    let mut resume = 0_u32;
    // `u64` keeps the buffer aligned for the structures EnumServicesStatusExW writes.
    let mut buffer = vec![0_u64; 32 * 1024];
    for _ in 0..64 {
        let (mut needed, mut returned) = (0_u32, 0_u32);
        let ok = unsafe {
            EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_ACTIVE,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut needed,
                &mut returned,
                &mut resume,
                null(),
            )
        };
        let more =
            ok == 0 && unsafe { GetLastError() } == windows_sys::Win32::Foundation::ERROR_MORE_DATA;
        if ok == 0 && !more {
            break;
        }
        let entries = unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                returned as usize,
            )
        };
        for entry in entries {
            let mut length = 0;
            while unsafe { *entry.lpServiceName.add(length) } != 0 {
                length += 1;
            }
            names.push(String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(entry.lpServiceName, length)
            }));
        }
        if !more {
            break;
        }
    }
    names
}

/// The allowlisted packet-filter services that exist, and whether each runs.
fn dpi_services() -> Vec<DpiServiceState> {
    let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Vec::new();
    }
    let manager = ScHandle(manager);
    DpiService::ALL
        .into_iter()
        .filter_map(|service| {
            let name = wide(service.service_name());
            let handle = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_QUERY_STATUS) };
            if handle.is_null() {
                return None;
            }
            let handle = ScHandle(handle);
            let mut status: SERVICE_STATUS = unsafe { zeroed() };
            let running = unsafe { QueryServiceStatus(handle.0, &mut status) } != 0
                && status.dwCurrentState != SERVICE_STOPPED;
            Some(DpiServiceState { service, running })
        })
        .collect()
}

fn dpi_folders() -> Vec<String> {
    [&FOLDERID_Downloads, &FOLDERID_Desktop]
        .into_iter()
        .filter_map(known_folder)
        .flat_map(|root| DPI_FOLDERS.map(|name| root.join(name)))
        .filter(|path| path.is_dir())
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// Process image names (lower case) by PID.
fn process_names() -> HashMap<u32, String> {
    let mut names = HashMap::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return names;
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        names.insert(
            entry.th32ProcessID,
            from_wide_nul(&entry.szExeFile).to_lowercase(),
        );
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    names
}

fn process_image(pid: u32) -> Option<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return None;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    (unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            PROCESS_NAME_WIN32,
            buffer.as_mut_ptr(),
            &mut length,
        )
    } != 0)
        .then(|| String::from_utf16_lossy(&buffer[..length as usize]))
}

/// One of `GetExtendedTcpTable`/`GetExtendedUdpTable` into a `u32`-aligned buffer.
fn ip_table(call: impl Fn(*mut core::ffi::c_void, &mut u32) -> u32) -> Option<Vec<u32>> {
    let mut size = 0_u32;
    for _ in 0..4 {
        let mut buffer = vec![0_u32; (size as usize).div_ceil(4).max(1)];
        let mut bytes = (buffer.len() * 4) as u32;
        match call(buffer.as_mut_ptr().cast(), &mut bytes) {
            ERROR_SUCCESS => return Some(buffer),
            ERROR_INSUFFICIENT_BUFFER => size = bytes + 1024,
            _ => return None,
        }
    }
    None
}

/// Rows of a table: a `u32` count, then `count` rows of `T` (all 4-byte aligned).
fn rows<T: Copy>(table: &[u32]) -> Vec<T> {
    let Some(&count) = table.first() else {
        return Vec::new();
    };
    let available = (table.len() * 4).saturating_sub(4) / size_of::<T>();
    let base = table.as_ptr().cast::<u8>().wrapping_add(4);
    (0..(count as usize).min(available))
        .map(|index| unsafe {
            std::ptr::read_unaligned(base.add(index * size_of::<T>()).cast::<T>())
        })
        .collect()
}

/// `dwLocalPort` holds the port in network byte order in its low 16 bits.
fn table_port(value: u32) -> u16 {
    u16::from_be(value as u16)
}

/// Who listens on the wanted ports (TCP listeners and UDP sockets, IPv4 and IPv6).
fn listeners(wanted: &[ListenPort]) -> Vec<PortOwner> {
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    let mut found = Vec::new();
    for family in [AF_INET, AF_INET6] {
        if let Some(table) = ip_table(|buffer, size| unsafe {
            GetExtendedTcpTable(buffer, size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0)
        }) {
            if family == AF_INET {
                found.extend(
                    rows::<MIB_TCPROW_OWNER_PID>(&table)
                        .iter()
                        .map(|row| (table_port(row.dwLocalPort), false, row.dwOwningPid)),
                );
            } else {
                found.extend(
                    rows::<MIB_TCP6ROW_OWNER_PID>(&table)
                        .iter()
                        .map(|row| (table_port(row.dwLocalPort), false, row.dwOwningPid)),
                );
            }
        }
        if let Some(table) = ip_table(|buffer, size| unsafe {
            GetExtendedUdpTable(buffer, size, 0, family, UDP_TABLE_OWNER_PID, 0)
        }) {
            if family == AF_INET {
                found.extend(
                    rows::<MIB_UDPROW_OWNER_PID>(&table)
                        .iter()
                        .map(|row| (table_port(row.dwLocalPort), true, row.dwOwningPid)),
                );
            } else {
                found.extend(
                    rows::<MIB_UDP6ROW_OWNER_PID>(&table)
                        .iter()
                        .map(|row| (table_port(row.dwLocalPort), true, row.dwOwningPid)),
                );
            }
        }
    }
    found.retain(|(port, udp, _)| {
        wanted
            .iter()
            .any(|wanted| wanted.port == *port && wanted.udp == *udp)
    });
    if found.is_empty() {
        return Vec::new();
    }
    let names = process_names();
    found
        .into_iter()
        .map(|(port, udp, pid)| PortOwner {
            port,
            udp,
            process_name: names.get(&pid).cloned(),
            image: process_image(pid),
        })
        .collect()
}

/// Puts back MULTICORE's leftover proxy, then switches off whatever proxy is still on (the
/// daemon is disconnected here, so no proxy is ours to keep). Returns what was done and the
/// host of a proxy auto-config script, which is reported but left alone.
/// The outcome, and the host of a proxy auto-configuration script when one is set (`None`
/// inside when its address has no recognizable host).
fn clear_system_proxy(marker: &Path) -> (ProxyOutcome, Option<Option<String>>) {
    let settings = WinInetSettings;
    // MULTICORE's own proxy from a connection that never cleaned up: the marker still matches
    // what is applied, so `recover` puts the user's earlier settings back.
    let ours_left_over = std::fs::read(marker)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<crate::system_proxy::ProxyMarker>(&bytes).ok())
        .zip(settings.read().ok())
        .is_some_and(|(saved, current)| {
            crate::system_proxy::restore_plan(&current, &saved).is_some()
        });
    let recovered = marker.exists()
        && crate::system_proxy::recover(&settings, marker).is_ok()
        && ours_left_over;
    let Ok(current) = settings.read() else {
        return (ProxyOutcome::Direct, None);
    };
    let pac_host = current
        .auto_config_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
        .map(|url| {
            reqwest::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
        });
    if current.proxy_enable != Some(1) {
        let outcome = if recovered {
            ProxyOutcome::StaleOursCleared
        } else {
            ProxyOutcome::Direct
        };
        return (outcome, pac_host);
    }
    let server = current.proxy_server.clone().unwrap_or_default();
    let ours = looks_like_our_proxy(&server, current.proxy_override.as_deref());
    let mut disabled = current.clone();
    disabled.proxy_enable = Some(0);
    let outcome = if settings.write(&disabled).is_ok() {
        settings.notify_changed();
        if ours {
            ProxyOutcome::StaleOursCleared
        } else {
            ProxyOutcome::ForeignDisabled { server }
        }
    } else {
        ProxyOutcome::DisableFailed { server }
    };
    (outcome, pac_host)
}

fn ras_connections() -> Vec<String> {
    let mut connections = vec![unsafe { zeroed::<RASCONNW>() }; 1];
    connections[0].dwSize = size_of::<RASCONNW>() as u32;
    let mut bytes = size_of::<RASCONNW>() as u32;
    let mut count = 0_u32;
    let mut status =
        unsafe { RasEnumConnectionsW(connections.as_mut_ptr(), &mut bytes, &mut count) };
    if status == ERROR_BUFFER_TOO_SMALL {
        let wanted = (bytes as usize)
            .div_ceil(size_of::<RASCONNW>())
            .clamp(1, 64);
        connections = vec![unsafe { zeroed::<RASCONNW>() }; wanted];
        connections[0].dwSize = size_of::<RASCONNW>() as u32;
        bytes = (wanted * size_of::<RASCONNW>()) as u32;
        status = unsafe { RasEnumConnectionsW(connections.as_mut_ptr(), &mut bytes, &mut count) };
    }
    if status != ERROR_SUCCESS {
        return Vec::new();
    }
    connections
        .iter()
        .take(count as usize)
        .map(|connection| from_wide_nul(&connection.szEntryName))
        .filter(|name| !name.is_empty())
        .collect()
}

/// Drover's files under `%LOCALAPPDATA%\<Discord flavour>\app-*\`, regular files only.
fn drover_files() -> Vec<PathBuf> {
    let Some(local) = known_folder(&FOLDERID_LocalAppData) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for flavour in DISCORD_FOLDERS {
        let Ok(entries) = std::fs::read_dir(local.join(flavour)) else {
            continue;
        };
        for entry in entries.flatten() {
            let is_app = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.to_ascii_lowercase().starts_with("app-"));
            // `file_type` does not follow links: a junction is never descended into.
            if !is_app || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            for name in DROVER_FILES {
                let path = entry.path().join(name);
                if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                    files.push(path);
                }
            }
        }
    }
    files
}

fn discord_running() -> bool {
    process_names()
        .values()
        .any(|name| DISCORD_IMAGES.contains(&name.as_str()))
}

fn scan_drover() -> Option<DroverState> {
    let files = drover_files();
    (!files.is_empty()).then(|| DroverState {
        files: files.len(),
        discord_running: discord_running(),
    })
}

/// Deletes Drover's files (found again now, never taken from the request).
fn remove_drover() -> DroverRemoval {
    let files = drover_files();
    if files.is_empty() {
        return DroverRemoval::NothingFound;
    }
    if discord_running() {
        return DroverRemoval::DiscordRunning;
    }
    if files.iter().all(|file| std::fs::remove_file(file).is_ok()) {
        DroverRemoval::Removed
    } else {
        DroverRemoval::Failed
    }
}

struct RegistryKey(HKEY);
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// Network Connections policies set to 0 (a managed PC that forbids changing the network).
fn restrictive_policies() -> Vec<&'static str> {
    let mut key: HKEY = null_mut();
    let path = wide(r"SOFTWARE\Policies\Microsoft\Windows\Network Connections");
    if unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            path.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_WOW64_64KEY,
            &mut key,
        )
    } != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let key = RegistryKey(key);
    NETWORK_POLICY_VALUES
        .into_iter()
        .filter(|name| {
            let name = wide(name);
            let mut kind = 0_u32;
            let mut value = 0_u32;
            let mut size = size_of::<u32>() as u32;
            let status = unsafe {
                RegQueryValueExW(
                    key.0,
                    name.as_ptr(),
                    null(),
                    &mut kind,
                    (&raw mut value).cast(),
                    &mut size,
                )
            };
            status == ERROR_SUCCESS && kind == REG_DWORD && value == 0
        })
        .collect()
}

/// `System32\<tool> <arguments>` without a window, bounded in time; whether it succeeded.
fn system_tool(tool: &str, arguments: &[&str]) -> bool {
    let Some(directory) = system_directory() else {
        return false;
    };
    let Ok(mut child) = Command::new(directory.join(tool))
        .args(arguments)
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// `ipconfig /flushdns` (allowed for standard users).
fn flush_dns() -> bool {
    system_tool("ipconfig.exe", &["/flushdns"])
}

/// The machine-wide WinHTTP proxy, when one is set.
fn winhttp_proxy() -> Option<String> {
    let mut info: WINHTTP_PROXY_INFO = unsafe { zeroed() };
    if unsafe { WinHttpGetDefaultProxyConfiguration(&mut info) } == 0 {
        return None;
    }
    let read = |pointer: *mut u16| {
        if pointer.is_null() {
            return None;
        }
        let mut length = 0;
        while unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) });
        unsafe { GlobalFree(pointer.cast()) };
        Some(text)
    };
    let proxy = read(info.lpszProxy);
    let _ = read(info.lpszProxyBypass);
    proxy.filter(|proxy| {
        info.dwAccessType == WINHTTP_ACCESS_TYPE_NAMED_PROXY && !proxy.trim().is_empty()
    })
}

/// `multicore-core-host --troubleshoot <tokens>` next to this daemon, behind a UAC prompt.
pub struct UacHelper;

impl ElevatedFixes for UacHelper {
    fn run_fixes(&self, fixes: &[PrivilegedFix]) -> Result<Vec<FixOutcome>, HelperError> {
        let daemon = std::env::current_exe().map_err(|_| HelperError::Failed)?;
        let host = crate::elevated_controller::packaged_core_host_from(&daemon)
            .map_err(|_| HelperError::Failed)?;
        let directory = host.parent().ok_or(HelperError::Failed)?.to_path_buf();
        // Fixed tokens without spaces or quotes: no quoting needed.
        let parameters = wide(helper_arguments(fixes).join(" "));
        let (file, verb, directory) = (wide(&host), wide("runas"), wide(&directory));
        let mut info: SHELLEXECUTEINFOW = unsafe { zeroed() };
        info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        info.lpVerb = verb.as_ptr();
        info.lpFile = file.as_ptr();
        info.lpParameters = parameters.as_ptr();
        info.lpDirectory = directory.as_ptr();
        info.nShow = SW_HIDE;
        if unsafe { ShellExecuteExW(&mut info) } == 0 {
            // 1223: the user dismissed the prompt.
            return Err(if unsafe { GetLastError() } == 1223 {
                HelperError::Cancelled
            } else {
                HelperError::Failed
            });
        }
        if info.hProcess.is_null() {
            return Err(HelperError::Failed);
        }
        let process = info.hProcess;
        let finished = unsafe { WaitForSingleObject(process, HELPER_TIMEOUT.as_millis() as u32) }
            == WAIT_OBJECT_0;
        let mut code = 0_u32;
        let read = finished && unsafe { GetExitCodeProcess(process, &mut code) } != 0;
        unsafe { CloseHandle(process) };
        if !read {
            return Err(HelperError::Failed);
        }
        decode_helper_exit(code, fixes.len()).ok_or(HelperError::Failed)
    }
}
