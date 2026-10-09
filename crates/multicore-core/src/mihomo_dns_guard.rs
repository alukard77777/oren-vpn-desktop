//! DNS safety net for provider Mihomo configs, applied by the runtime overlay.
//!
//! It only fills in what a provider left out or repairs a pool its own TUN would black-hole;
//! anything the provider set explicitly stays as written.

use std::{collections::BTreeSet, net::IpAddr};

use serde_yaml_ng::{Mapping, Value};

/// Names that must resolve to real addresses under fake-ip, or the system looks broken while
/// connected: Windows' connectivity check (NCSI) reports «No internet», the clock never syncs
/// (NTP), local names and mDNS stop working, and WebRTC's NAT discovery (STUN) fails.
/// Only used when a fake-ip config ships no `fake-ip-filter` of its own.
const DEFAULT_FAKE_IP_FILTER: &[&str] = &[
    "*.lan",
    "*.local",
    "*.localdomain",
    "*.arpa",
    "+.home.arpa",
    "localhost.ptlogin2.qq.com",
    "+.msftconnecttest.com",
    "+.msftncsi.com",
    "time.*.com",
    "time.*.gov",
    "time.*.apple.com",
    "time-ios.apple.com",
    "+.time.windows.com",
    "ntp.*.com",
    "+.pool.ntp.org",
    "+.stun.*.*",
    "+.stun.*.*.*",
    "+.stun.*.*.*.*",
];

/// Mihomo's TUN address when `tun.inet6-address` is not set.
const MIHOMO_DEFAULT_INET6_ADDRESS: &str = "fdfe:dcba:9876::1/126";

fn key(name: &str) -> Value {
    Value::String(name.to_owned())
}

/// Applies the safety net to a Mihomo document's top-level mapping.
pub(crate) fn apply(mapping: &mut Mapping) {
    let ipv6_off = mapping.get(key("ipv6")).and_then(Value::as_bool) == Some(false);
    let tun = mapping.get(key("tun")).and_then(Value::as_mapping).cloned();
    let Some(dns) = mapping.get_mut(key("dns")).and_then(Value::as_mapping_mut) else {
        return;
    };
    let fake_ip = dns
        .get(key("enhanced-mode"))
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("fake-ip"));
    if !fake_ip {
        return;
    }
    fill_fake_ip_filter(dns);
    // With `ipv6: false` Mihomo discards the v6 pool itself.
    if !ipv6_off {
        drop_unroutable_v6_pool(dns, tun.as_ref());
    }
}

/// Makes Mihomo answer `domains` (exact names) with real addresses in fake-ip mode: they are
/// added to a blacklist filter, prepended as `real-ip` rules in rule mode, and taken out of a
/// whitelist. A whitelist wildcard that also covers one of them is left alone: removing it would
/// take real addresses from every other name it covers.
pub(crate) fn exempt_from_fake_ip(dns: &mut Mapping, domains: &BTreeSet<String>) {
    let fake_ip = dns
        .get(key("enhanced-mode"))
        .and_then(Value::as_str)
        .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("fake-ip"));
    if !fake_ip || domains.is_empty() {
        return;
    }
    let mode = match dns.get(key("fake-ip-filter-mode")) {
        None | Some(Value::Null) => "blacklist".to_owned(),
        Some(mode) => match mode.as_str() {
            Some(mode) => mode.trim().to_ascii_lowercase(),
            None => return,
        },
    };
    let filter = dns.entry(key("fake-ip-filter")).or_insert(Value::Null);
    if filter.is_null() {
        *filter = Value::Sequence(Vec::new());
    }
    let Some(filter) = filter.as_sequence_mut() else {
        return;
    };
    let listed = |filter: &Vec<Value>, wanted: &str| {
        filter.iter().any(|entry| {
            entry
                .as_str()
                .is_some_and(|entry| entry.trim().eq_ignore_ascii_case(wanted))
        })
    };
    match mode.as_str() {
        "blacklist" => {
            for domain in domains {
                if !listed(filter, domain) {
                    filter.push(key(domain));
                }
            }
        }
        "rule" => {
            let rules: Vec<Value> = domains
                .iter()
                .map(|domain| format!("DOMAIN,{domain},real-ip"))
                .filter(|rule| !listed(filter, rule))
                .map(Value::String)
                .collect();
            filter.splice(0..0, rules);
        }
        "whitelist" => filter.retain(|entry| {
            !entry.as_str().is_some_and(|entry| {
                domains
                    .iter()
                    .any(|domain| entry.trim().eq_ignore_ascii_case(domain))
            })
        }),
        _ => {}
    }
}

/// Without a filter, NCSI/NTP/LAN names get fake addresses. In whitelist or rule mode the list
/// means something else, so only Mihomo's default (blacklist) mode is filled.
fn fill_fake_ip_filter(dns: &mut Mapping) {
    if dns
        .get(key("fake-ip-filter"))
        .is_some_and(|value| !value.is_null())
    {
        return;
    }
    let blacklist = match dns.get(key("fake-ip-filter-mode")) {
        None | Some(Value::Null) => true,
        Some(mode) => mode
            .as_str()
            .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("blacklist")),
    };
    if !blacklist {
        return;
    }
    dns.insert(
        key("fake-ip-filter"),
        Value::Sequence(
            DEFAULT_FAKE_IP_FILTER
                .iter()
                .map(|entry| key(entry))
                .collect(),
        ),
    );
}

/// A `fake-ip-range6` the TUN does not route is worse than none: Windows sends those
/// connections out of the physical adapter, where nothing answers, and every dual-stack site
/// stalls on Happy Eyeballs. Without the pool Mihomo answers AAAA with no records, so apps use
/// their working IPv4 fake address. Measured by SlothClash (PR #50) with the common
/// `route-exclude-address: [..., fc00::/7]` list, which covers every ULA pool.
fn drop_unroutable_v6_pool(dns: &mut Mapping, tun: Option<&Mapping>) {
    let Some(pool) = dns
        .get(key("fake-ip-range6"))
        .and_then(Value::as_str)
        .and_then(Prefix::parse)
        .filter(|pool| pool.is_v6())
    else {
        return;
    };
    let empty = Mapping::new();
    let tun = tun.unwrap_or(&empty);
    // With auto-route off Mihomo installs no routes; whoever turned it off owns the table.
    if tun.get(key("auto-route")).and_then(Value::as_bool) == Some(false) {
        return;
    }
    let on_link = match tun.get(key("inet6-address")) {
        None | Some(Value::Null) => vec![
            Prefix::parse(MIHOMO_DEFAULT_INET6_ADDRESS)
                .expect("Mihomo's default inet6-address parses"),
        ],
        Some(value) => prefixes(value),
    };
    // A TUN address covering the pool makes it on-link on the tunnel itself.
    if on_link.iter().any(|address| address.covers(&pool)) {
        return;
    }
    let excluded = ["route-exclude-address", "inet6-route-exclude-address"]
        .iter()
        .filter_map(|name| tun.get(key(name)))
        .flat_map(prefixes)
        .any(|excluded| excluded.covers(&pool));
    // An explicit route list routes only what it names.
    let routes: Vec<Prefix> = ["route-address", "inet6-route-address"]
        .iter()
        .filter_map(|name| tun.get(key(name)))
        .flat_map(prefixes)
        .filter(Prefix::is_v6)
        .collect();
    let unrouted = !routes.is_empty() && !routes.iter().any(|route| route.covers(&pool));
    if excluded || unrouted {
        dns.remove(key("fake-ip-range6"));
    }
}

/// The CIDR entries of a YAML list (or a single string); anything unparsable is skipped.
fn prefixes(value: &Value) -> Vec<Prefix> {
    match value {
        Value::String(text) => Prefix::parse(text).into_iter().collect(),
        Value::Sequence(entries) => entries
            .iter()
            .filter_map(Value::as_str)
            .filter_map(Prefix::parse)
            .collect(),
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Prefix {
    address: IpAddr,
    length: u8,
}

impl Prefix {
    fn parse(text: &str) -> Option<Self> {
        let (address, length) = text.trim().split_once('/')?;
        let address: IpAddr = address.trim().parse().ok()?;
        let length: u8 = length.trim().parse().ok()?;
        let maximum = if address.is_ipv4() { 32 } else { 128 };
        (length <= maximum).then_some(Self { address, length })
    }

    fn is_v6(&self) -> bool {
        self.address.is_ipv6()
    }

    /// Whether every address of `inner` lies in `self` (same family only).
    fn covers(&self, inner: &Self) -> bool {
        if self.length > inner.length {
            return false;
        }
        match (self.address, inner.address) {
            (IpAddr::V4(outer), IpAddr::V4(inner)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.length))
                    .unwrap_or(0);
                u32::from(outer) & mask == u32::from(inner) & mask
            }
            (IpAddr::V6(outer), IpAddr::V6(inner)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.length))
                    .unwrap_or(0);
                u128::from(outer) & mask == u128::from(inner) & mask
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Prefix;

    fn prefix(text: &str) -> Prefix {
        Prefix::parse(text).unwrap()
    }

    #[test]
    fn prefix_cover_is_by_family_and_length() {
        assert!(prefix("fc00::/7").covers(&prefix("fdfe:dcba:9876::1/64")));
        assert!(prefix("::/0").covers(&prefix("fdfe:dcba:9876::1/64")));
        assert!(!prefix("fdfe:dcba:9876::1/126").covers(&prefix("fdfe:dcba:9876::1/64")));
        assert!(!prefix("2001:db8::/32").covers(&prefix("fdfe:dcba:9876::1/64")));
        assert!(prefix("0.0.0.0/0").covers(&prefix("198.18.0.1/16")));
        assert!(!prefix("0.0.0.0/0").covers(&prefix("fdfe::/64")));
        assert!(Prefix::parse("fc00::/129").is_none());
        assert!(Prefix::parse("fc00::").is_none());
    }
}
