use std::collections::{HashMap, HashSet};

use serde_json::Value as JsonValue;
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;

pub const MAX_CONFIG_BYTES: usize = 32 * 1024 * 1024;
const MAX_GROUP_ICON_FIELD_BYTES: usize = 2048;

/// Xray config for a Mihomo-only subscription (its YAML has no loopback SOCKS
/// bridge to Xray). Nothing routes into it: Xray only idles, so connect and
/// disconnect keep one two-core lifecycle.
pub const IDLE_XRAY_OUTBOUND_TAG: &str = "multicore-idle";
pub const IDLE_XRAY_CONFIG: &[u8] = br#"{"log":{"loglevel":"warning"},"inbounds":[],"outbounds":[{"tag":"multicore-idle","protocol":"freedom"}]}"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyGroup {
    pub name: String,
    pub group_type: String,
    /// The group's explicit `proxies` list, as written.
    pub proxies: Vec<String>,
    /// The group's `icon` value, as written by the provider (usually an image URL).
    /// Untrusted: only `Snapshot` decides whether it is a fetchable HTTPS URL.
    pub icon: Option<String>,
    /// `hidden: true`: the provider keeps the group out of client UIs (Mihomo's API reports it
    /// as hidden; dashboards, Clash Verge and FlClash do not list it).
    pub hidden: bool,
    /// What Mihomo puts in the group, in its order, without duplicates: the explicit `proxies`,
    /// then (`include-all` / `include-all-proxies`) every config proxy matching `filter`.
    /// Members dropped by `exclude-filter` / `exclude-type` stay listed but not `offered`, so
    /// a member keeps its position whatever the filters say.
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    pub name: String,
    /// Mihomo keeps the member after `exclude-filter` and `exclude-type`: it can be selected.
    pub offered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MihomoSocksMapping {
    pub name: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MihomoConfig {
    raw: String,
    document: YamlValue,
    proxy_names: Vec<String>,
    groups: Vec<ProxyGroup>,
    loopback_socks_mappings: Vec<MihomoSocksMapping>,
}

impl MihomoConfig {
    pub fn raw_yaml(&self) -> &str {
        &self.raw
    }

    pub fn document(&self) -> &YamlValue {
        &self.document
    }

    pub fn proxy_names(&self) -> &[String] {
        &self.proxy_names
    }

    pub fn groups(&self) -> &[ProxyGroup] {
        &self.groups
    }

    pub fn loopback_socks_mappings(&self) -> &[MihomoSocksMapping] {
        &self.loopback_socks_mappings
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XrayConfig {
    raw: Vec<u8>,
}

impl XrayConfig {
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn raw_json(&self) -> &str {
        // Construction validates UTF-8 and the bytes never change afterwards.
        std::str::from_utf8(&self.raw).expect("validated UTF-8")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    #[error("configuration exceeds the 32 MiB limit")]
    TooLarge,
    #[error("configuration is not UTF-8")]
    InvalidUtf8,
    #[error("expected a Mihomo YAML mapping")]
    InvalidMihomo,
    #[error("expected an Xray JSON object")]
    InvalidXray,
}

pub fn parse_mihomo(bytes: &[u8]) -> Result<MihomoConfig, ConfigError> {
    check_size(bytes)?;
    let raw = std::str::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8)?;
    let trimmed = raw.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return Err(ConfigError::InvalidMihomo);
    }

    let document: YamlValue =
        serde_yaml_ng::from_str(raw).map_err(|_| ConfigError::InvalidMihomo)?;
    let mapping = document.as_mapping().ok_or(ConfigError::InvalidMihomo)?;

    const MIHOMO_ROOT_KEYS: &[&str] = &[
        "proxies",
        "proxy-groups",
        "rules",
        "rule-providers",
        "proxy-providers",
        "tun",
        "dns",
        "mixed-port",
        "socks-port",
        "redir-port",
        "tproxy-port",
        "listeners",
        "mode",
    ];
    if !MIHOMO_ROOT_KEYS
        .iter()
        .any(|key| mapping.contains_key(YamlValue::String((*key).into())))
    {
        return Err(ConfigError::InvalidMihomo);
    }

    validate_mihomo_sequence(mapping, "rules")?;
    validate_mihomo_sequence(mapping, "proxies")?;
    validate_mihomo_sequence(mapping, "proxy-groups")?;

    if let Some(proxies) = mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
    {
        for proxy in proxies {
            if string_field(proxy, "name").is_none() || string_field(proxy, "type").is_none() {
                return Err(ConfigError::InvalidMihomo);
            }
        }
    }
    if let Some(groups) = mapping
        .get(YamlValue::String("proxy-groups".into()))
        .and_then(YamlValue::as_sequence)
    {
        for group in groups {
            if string_field(group, "name").is_none() || string_field(group, "type").is_none() {
                return Err(ConfigError::InvalidMihomo);
            }
        }
    }

    let proxy_names: Vec<String> = mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|proxy| string_field(proxy, "name"))
        .map(ToOwned::to_owned)
        .collect();
    let mut member_types: HashMap<&str, &str> = HashMap::new();
    for proxy in mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
    {
        if let (Some(name), Some(kind)) = (string_field(proxy, "name"), string_field(proxy, "type"))
        {
            member_types
                .entry(name)
                .or_insert_with(|| mihomo_type_name(kind));
        }
    }
    for group in mapping
        .get(YamlValue::String("proxy-groups".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
    {
        if let (Some(name), Some(kind)) = (string_field(group, "name"), string_field(group, "type"))
        {
            member_types
                .entry(name)
                .or_insert_with(|| mihomo_type_name(kind));
        }
    }
    // Mihomo's `AllProxies`: every `proxies` entry, sorted (Go's byte-wise `slices.Sort`).
    let mut all_proxies: Vec<&str> = proxy_names.iter().map(String::as_str).collect();
    all_proxies.sort_unstable();
    all_proxies.dedup();

    let groups = mapping
        .get(YamlValue::String("proxy-groups".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|group| {
            let name = string_field(group, "name")?.to_owned();
            let group_type = string_field(group, "type")?.to_owned();
            let proxies: Vec<String> = group
                .as_mapping()?
                .get(YamlValue::String("proxies".into()))
                .and_then(YamlValue::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(YamlValue::as_str)
                .map(ToOwned::to_owned)
                .collect();
            let icon = string_field(group, "icon")
                .map(str::trim)
                .filter(|icon| !icon.is_empty() && icon.len() <= MAX_GROUP_ICON_FIELD_BYTES)
                .map(ToOwned::to_owned);
            let members = group_members(group, &proxies, &all_proxies, &member_types);
            Some(ProxyGroup {
                name,
                group_type,
                proxies,
                icon,
                hidden: bool_field(group, "hidden"),
                members,
            })
        })
        .collect();

    let mut seen_mappings = HashSet::new();
    let loopback_socks_mappings = mapping
        .get(YamlValue::String("proxies".into()))
        .and_then(YamlValue::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|proxy| {
            let name = string_field(proxy, "name")?;
            if !string_field(proxy, "type")?.eq_ignore_ascii_case("socks5")
                || string_field(proxy, "server")? != "127.0.0.1"
            {
                return None;
            }
            let port = proxy
                .as_mapping()?
                .get(YamlValue::String("port".into()))?
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())?;
            if port == 0 || !seen_mappings.insert(name) {
                return None;
            }
            Some(MihomoSocksMapping {
                name: name.to_owned(),
                port,
            })
        })
        .collect();

    Ok(MihomoConfig {
        raw: raw.to_owned(),
        document,
        proxy_names,
        groups,
        loopback_socks_mappings,
    })
}

pub fn parse_xray(bytes: &[u8]) -> Result<XrayConfig, ConfigError> {
    check_size(bytes)?;
    let raw = std::str::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8)?;
    let document: JsonValue = serde_json::from_str(raw).map_err(|_| ConfigError::InvalidXray)?;
    if !document.is_object() {
        return Err(ConfigError::InvalidXray);
    }
    Ok(XrayConfig {
        raw: bytes.to_vec(),
    })
}

fn check_size(bytes: &[u8]) -> Result<(), ConfigError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        Err(ConfigError::TooLarge)
    } else {
        Ok(())
    }
}

/// Longest `filter` / `exclude-filter` pattern evaluated; a longer one is ignored.
const MAX_GROUP_PATTERN_BYTES: usize = 4096;
/// Compiled-size cap per pattern: provider patterns are untrusted.
const MAX_GROUP_PATTERN_COMPILED_BYTES: usize = 1 << 20;

/// See `ProxyGroup::members`. Mirrors Mihomo v1.19.32 `outboundgroup.ParseProxyGroup` and
/// `GroupBase.GetProxies` for config proxies; `use:` providers are fetched by Mihomo at run
/// time and cannot be listed here.
fn group_members(
    group: &YamlValue,
    explicit: &[String],
    all_proxies: &[&str],
    member_types: &HashMap<&str, &str>,
) -> Vec<GroupMember> {
    let mut seen = HashSet::new();
    let mut names: Vec<&str> = explicit
        .iter()
        .map(String::as_str)
        .filter(|name| seen.insert(*name))
        .collect();
    if bool_field(group, "include-all") || bool_field(group, "include-all-proxies") {
        match string_field(group, "filter").filter(|filter| !filter.is_empty()) {
            None => names.extend(all_proxies.iter().filter(|name| seen.insert(**name))),
            Some(filter) => {
                // A pattern Rust cannot evaluate adds nothing (as before this was modelled).
                if let Some(filters) = compile_group_patterns(filter) {
                    for name in all_proxies {
                        if filters.iter().any(|filter| filter.is_match(name)) && seen.insert(name) {
                            names.push(name);
                        }
                    }
                }
            }
        }
    }
    // A pattern Rust cannot evaluate excludes nothing: the member stays selectable as before.
    let exclude_filters = string_field(group, "exclude-filter")
        .filter(|filter| !filter.is_empty())
        .and_then(compile_group_patterns)
        .unwrap_or_default();
    let exclude_types: Vec<&str> = string_field(group, "exclude-type")
        .map(|types| {
            types
                .split('|')
                .map(str::trim)
                .filter(|kind| !kind.is_empty())
                .collect()
        })
        .unwrap_or_default();
    names
        .into_iter()
        .map(|name| {
            let filtered = exclude_filters.iter().any(|filter| filter.is_match(name));
            let kind = member_types
                .get(name)
                .copied()
                .unwrap_or_else(|| builtin_type_name(name));
            let excluded_type = exclude_types
                .iter()
                .any(|excluded| excluded.eq_ignore_ascii_case(kind));
            GroupMember {
                name: name.to_owned(),
                offered: !filtered && !excluded_type,
            }
        })
        .collect()
}

/// Mihomo splits `filter` / `exclude-filter` on backticks into several regular expressions.
/// `None` when any of them cannot be compiled by Rust's `regex` (no look-around or
/// back-references, unlike Mihomo's regexp2) or is unreasonably large.
fn compile_group_patterns(patterns: &str) -> Option<Vec<regex::Regex>> {
    if patterns.len() > MAX_GROUP_PATTERN_BYTES {
        return None;
    }
    patterns
        .split('`')
        .map(|pattern| {
            regex::RegexBuilder::new(pattern)
                .size_limit(MAX_GROUP_PATTERN_COMPILED_BYTES)
                .dfa_size_limit(MAX_GROUP_PATTERN_COMPILED_BYTES)
                .build()
                .ok()
        })
        .collect()
}

/// Mihomo's adapter type name (`C.AdapterType.String()`) for a config `type`, which is what
/// `exclude-type` is compared with (case-insensitively).
fn mihomo_type_name(kind: &str) -> &str {
    match kind.to_ascii_lowercase().as_str() {
        "ss" => "Shadowsocks",
        "ssr" => "ShadowsocksR",
        "socks5" => "Socks5",
        "http" => "Http",
        "hysteria2" | "hy2" => "Hysteria2",
        "select" => "Selector",
        "url-test" => "URLTest",
        "fallback" => "Fallback",
        "load-balance" => "LoadBalance",
        "relay" => "Relay",
        _ => kind,
    }
}

fn builtin_type_name(name: &str) -> &'static str {
    match name {
        "DIRECT" => "Direct",
        "REJECT" => "Reject",
        "REJECT-DROP" => "RejectDrop",
        "PASS" => "Pass",
        "COMPATIBLE" => "Compatible",
        "GLOBAL" => "Selector",
        _ => "",
    }
}

/// A YAML boolean, also accepting Mihomo's weakly typed "true"/"false" strings.
fn bool_field(value: &YamlValue, key: &str) -> bool {
    match value
        .as_mapping()
        .and_then(|map| map.get(YamlValue::String(key.into())))
    {
        Some(YamlValue::Bool(value)) => *value,
        Some(YamlValue::String(value)) => value.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn string_field<'a>(value: &'a YamlValue, key: &str) -> Option<&'a str> {
    value
        .as_mapping()?
        .get(YamlValue::String(key.into()))?
        .as_str()
}

fn validate_mihomo_sequence(
    mapping: &serde_yaml_ng::Mapping,
    key: &str,
) -> Result<(), ConfigError> {
    if mapping
        .get(YamlValue::String(key.into()))
        .is_some_and(|value| !value.is_sequence())
    {
        Err(ConfigError::InvalidMihomo)
    } else {
        Ok(())
    }
}
