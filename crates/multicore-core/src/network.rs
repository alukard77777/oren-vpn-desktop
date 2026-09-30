use std::io;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DefaultRouteCandidate {
    alias: String,
    interface_index: u32,
    route_metric: u32,
    interface_metric: u32,
    prefix_length: u8,
    connected: bool,
    loopback: bool,
}

fn select_default_route(
    candidates: impl IntoIterator<Item = DefaultRouteCandidate>,
) -> io::Result<String> {
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.prefix_length == 0
                && candidate.connected
                && !candidate.loopback
                // Our own adapter, also under the numbered name Windows gives a second
                // or lingering instance ("MultiCore 2").
                && !candidate.alias.starts_with("MultiCore")
                && !candidate.alias.is_empty()
                && candidate.alias.len() <= 256
                && !candidate.alias.chars().any(char::is_control)
        })
        .min_by_key(|candidate| {
            (
                candidate
                    .route_metric
                    .saturating_add(candidate.interface_metric),
                candidate.interface_index,
            )
        })
        .map(|candidate| candidate.alias)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no safe connected IPv4 default interface",
            )
        })
}

#[cfg(windows)]
pub fn default_ipv4_interface() -> io::Result<String> {
    use std::{ffi::c_void, ptr, slice};

    use windows_sys::Win32::{
        Foundation::NO_ERROR,
        NetworkManagement::{
            IpHelper::{
                ConvertInterfaceLuidToAlias, FreeMibTable, GetIpForwardTable2, GetIpInterfaceEntry,
                MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW,
            },
            Ndis::IF_MAX_STRING_SIZE,
        },
        Networking::WinSock::AF_INET,
    };

    struct MibTable(*const c_void);
    impl Drop for MibTable {
        fn drop(&mut self) {
            // SAFETY: the pointer is returned by GetIpForwardTable2 and freed exactly once.
            unsafe { FreeMibTable(self.0) };
        }
    }

    let mut table: *mut MIB_IPFORWARD_TABLE2 = ptr::null_mut();
    // SAFETY: `table` is a valid out-pointer and AF_INET requests the IPv4 route table.
    let result = unsafe { GetIpForwardTable2(AF_INET, &mut table) };
    if result != NO_ERROR {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    if table.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows returned an empty IPv4 route table",
        ));
    }
    let _table = MibTable(table.cast());
    // SAFETY: Windows allocates `NumEntries` contiguous rows beginning at `Table`.
    let rows =
        unsafe { slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
    let mut candidates = Vec::new();
    for row in rows {
        let mut interface = MIB_IPINTERFACE_ROW {
            Family: AF_INET,
            InterfaceLuid: row.InterfaceLuid,
            ..Default::default()
        };
        // SAFETY: `interface` is initialized with family/LUID and is a valid writable row.
        if unsafe { GetIpInterfaceEntry(&mut interface) } != NO_ERROR {
            continue;
        }
        let mut alias = [0_u16; IF_MAX_STRING_SIZE as usize + 1];
        // SAFETY: the alias buffer is writable and its length matches the provided capacity.
        if unsafe {
            ConvertInterfaceLuidToAlias(&row.InterfaceLuid, alias.as_mut_ptr(), alias.len())
        } != NO_ERROR
        {
            continue;
        }
        let alias_length = alias
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(alias.len());
        let Ok(alias) = String::from_utf16(&alias[..alias_length]) else {
            continue;
        };
        candidates.push(DefaultRouteCandidate {
            alias,
            interface_index: row.InterfaceIndex,
            route_metric: row.Metric,
            interface_metric: interface.Metric,
            prefix_length: row.DestinationPrefix.PrefixLength,
            connected: interface.Connected,
            loopback: row.Loopback,
        });
    }
    select_default_route(candidates)
}

#[cfg(not(windows))]
const IFF_UP: u32 = 0x1;
#[cfg(not(windows))]
const IFF_LOOPBACK: u32 = 0x8;
#[cfg(not(windows))]
const RTF_UP: u32 = 0x1;
#[cfg(not(windows))]
const MAX_INTERFACE_NAME_BYTES: usize = 15;

#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinuxInterface {
    pub(crate) index: u32,
    pub(crate) flags: u32,
    pub(crate) carrier: bool,
}

#[cfg(not(windows))]
pub(crate) fn valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_INTERFACE_NAME_BYTES
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'/')
}

#[cfg(not(windows))]
pub(crate) fn parse_interface_flags(text: &str) -> Option<u32> {
    u32::from_str_radix(text.trim().strip_prefix("0x")?, 16).ok()
}

#[cfg(not(windows))]
pub(crate) fn interface_is_up(flags: u32) -> bool {
    flags & IFF_UP != 0
}

#[cfg(not(windows))]
pub(crate) fn read_linux_interface(name: &str) -> Option<LinuxInterface> {
    if !valid_interface_name(name) {
        return None;
    }
    let directory = std::path::Path::new("/sys/class/net").join(name);
    let index = std::fs::read_to_string(directory.join("ifindex"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let flags = parse_interface_flags(&std::fs::read_to_string(directory.join("flags")).ok()?)?;
    let carrier =
        std::fs::read_to_string(directory.join("carrier")).is_ok_and(|value| value.trim() == "1");
    Some(LinuxInterface {
        index,
        flags,
        carrier,
    })
}

#[cfg(not(windows))]
fn linux_route_candidates(
    route_table: &str,
    interface: impl Fn(&str) -> Option<LinuxInterface>,
) -> Vec<DefaultRouteCandidate> {
    route_table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 8 || !valid_interface_name(fields[0]) {
                return None;
            }
            let destination = u32::from_str_radix(fields[1], 16).ok()?;
            let route_flags = u32::from_str_radix(fields[3], 16).ok()?;
            let metric = fields[6].parse::<u32>().ok()?;
            let mask = u32::from_str_radix(fields[7], 16).ok()?;
            let state = interface(fields[0])?;
            let prefix_length = if destination == 0 {
                mask.count_ones() as u8
            } else {
                mask.count_ones().max(1) as u8
            };
            Some(DefaultRouteCandidate {
                alias: fields[0].to_owned(),
                interface_index: state.index,
                route_metric: metric,
                interface_metric: 0,
                prefix_length,
                connected: route_flags & RTF_UP != 0
                    && interface_is_up(state.flags)
                    && state.carrier,
                loopback: state.flags & IFF_LOOPBACK != 0,
            })
        })
        .collect()
}

#[cfg(not(windows))]
pub fn default_ipv4_interface() -> io::Result<String> {
    let route_table = std::fs::read_to_string("/proc/net/route")?;
    select_default_route(linux_route_candidates(&route_table, read_linux_interface))
}

#[cfg(test)]
mod tests {
    use super::{DefaultRouteCandidate, default_ipv4_interface, select_default_route};

    fn candidate(alias: &str, route_metric: u32, interface_metric: u32) -> DefaultRouteCandidate {
        DefaultRouteCandidate {
            alias: alias.to_owned(),
            interface_index: 6,
            route_metric,
            interface_metric,
            prefix_length: 0,
            connected: true,
            loopback: false,
        }
    }

    #[test]
    fn route_selection_uses_the_best_connected_default_outside_multicore() {
        let mut disconnected = candidate("Wi-Fi", 1, 1);
        disconnected.connected = false;
        let mut specific = candidate("Specific", 0, 0);
        specific.prefix_length = 24;
        let mut loopback = candidate("Loopback", 0, 0);
        loopback.loopback = true;

        assert_eq!(
            select_default_route(vec![
                candidate("MultiCore", 0, 0),
                candidate("MultiCore 2", 0, 0),
                disconnected,
                specific,
                loopback,
                candidate("Ethernet", 20, 15),
                candidate("Corporate VPN", 5, 10),
            ])
            .unwrap(),
            "Corporate VPN"
        );
    }

    #[test]
    fn route_selection_fails_without_one_safe_connected_default() {
        assert!(select_default_route(Vec::new()).is_err());
        let mut unsafe_alias = candidate("Ethernet\nInjected", 1, 1);
        unsafe_alias.interface_index = 7;
        assert!(select_default_route(vec![unsafe_alias]).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "reads the host Windows route table"]
    fn host_route_probe_returns_a_safe_interface_alias() {
        let alias = default_ipv4_interface().unwrap();
        assert!(!alias.is_empty());
        assert!(!alias.chars().any(char::is_control));
    }

    #[cfg(not(windows))]
    const ROUTE_TABLE: &str = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
wlan0\t00000000\t0102A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n\
eth0\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n\
eth0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
MultiCore\t00000000\t00000000\t0001\t0\t0\t0\t00000000\t0\t0\t0\n\
stale0\t00000000\t0103A8C0\t0002\t0\t0\t1\t00000000\t0\t0\t0\n\
broken line\n";

    #[cfg(not(windows))]
    fn interface(name: &str) -> Option<super::LinuxInterface> {
        let (index, flags, carrier) = match name {
            "wlan0" => (3, 0x1003, true),
            "eth0" => (2, 0x1003, true),
            "MultiCore" => (9, 0x1091, true),
            "stale0" => (5, 0x1003, true),
            "down0" => (6, 0x1002, false),
            "unplugged0" => (7, 0x1003, false),
            "lo" => (1, 0x9, true),
            _ => return None,
        };
        Some(super::LinuxInterface {
            index,
            flags,
            carrier,
        })
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_route_table_selects_the_lowest_metric_up_default_outside_multicore() {
        let candidates = super::linux_route_candidates(ROUTE_TABLE, interface);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.alias == "wlan0")
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.alias == "eth0" && candidate.prefix_length == 24)
        );
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.alias == "stale0" && !candidate.connected)
        );
        assert_eq!(select_default_route(candidates).unwrap(), "eth0");
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_route_table_rejects_down_loopback_and_unknown_interfaces() {
        let table = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
down0\t00000000\t0102A8C0\t0003\t0\t0\t1\t00000000\n\
unplugged0\t00000000\t0102A8C0\t0003\t0\t0\t1\t00000000\n\
lo\t00000000\t00000000\t0001\t0\t0\t1\t00000000\n\
ghost\t00000000\t0102A8C0\t0003\t0\t0\t1\t00000000\n\
../etc\t00000000\t0102A8C0\t0003\t0\t0\t1\t00000000\n";
        assert!(select_default_route(super::linux_route_candidates(table, interface)).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn linux_interface_names_and_flags_are_bounded() {
        assert!(super::valid_interface_name("wlan0"));
        assert!(super::valid_interface_name("MultiCore"));
        for invalid in ["", ".", "..", "a/b", "sixteen-chars-xx", "tab\tname"] {
            assert!(!super::valid_interface_name(invalid), "{invalid:?}");
        }
        assert_eq!(super::parse_interface_flags("0x1003\n"), Some(0x1003));
        assert_eq!(super::parse_interface_flags("1003"), None);
        assert_eq!(super::parse_interface_flags("0xZZ"), None);
    }

    #[cfg(not(windows))]
    #[test]
    #[ignore = "reads the host Linux route table"]
    fn host_linux_route_probe_returns_a_safe_interface_name() {
        let name = default_ipv4_interface().unwrap();
        assert!(super::valid_interface_name(&name));
    }
}
