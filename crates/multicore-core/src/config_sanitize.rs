//! Turns a runtime generation staged by the (untrusted) user daemon into configs the privileged
//! service may run: every listener on loopback, the controller and its secret owned by the
//! service, every provider cache inside the service directory, and no file the user named.
//!
//! Listening ports (plan amendment A6): a generation staged for system-proxy mode (`tun.enable`
//! explicitly `false`, as `MihomoRuntimeControl::with_proxy_mode` writes it) keeps exactly one
//! loopback `mixed-port`; any other generation is a TUN generation and keeps no inbound port at
//! all. In proxy mode that one loopback port is reachable by every local user while it is open.
//!
//! Mihomo input is normalised before any rule is applied: YAML merge keys (`<<`) are resolved and
//! aliases expanded (the output is plain YAML without `<<`, anchors or aliases), custom tags are
//! refused, and only the top-level keys in [`ALLOWED_MIHOMO_KEYS`] survive. Xray keys are matched
//! with ASCII case folding, as Go's `encoding/json` does, and an object with two keys that fold
//! to the same name is refused.
//!
//! Xray is shaped, not deny-listed: only [`ALLOWED_XRAY_SECTIONS`] survive; every inbound must
//! be exactly a loopback SOCKS bridge ([`ALLOWED_XRAY_INBOUND_KEYS`], no `streamSettings`);
//! routing rules keep only matchers and targets ([`ALLOWED_XRAY_RULE_KEYS`], so no `webhook`);
//! outbounds keep only their documented fields and a known protocol. At any depth, server-only
//! features ([`XRAY_FORBIDDEN_KEYS`]) and file reads are refused, and no endpoint value
//! ([`XRAY_ENDPOINT_KEYS`]) may name a local socket or file.

use std::{net::IpAddr, path::Path};

use serde_yaml_ng::{Mapping, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SanitizeError {
    #[error("config is not a valid document")]
    Malformed,
    #[error("proxy-mode config has no loopback mixed port")]
    MissingMixedPort,
    #[error("config references a local file through {0}")]
    FileReference(String),
    #[error("config listens outside loopback through {0}")]
    NonLoopbackListener(String),
    #[error("service directory is not an absolute UTF-8 path")]
    ServiceDirectory,
    #[error("config uses a protocol the service does not run through {0}")]
    ForbiddenProtocol(String),
    #[error("config uses a field the service does not run through {0}")]
    ForbiddenField(String),
    #[error("config points an endpoint at a local socket or file through {0}")]
    LocalEndpoint(String),
}

pub struct MihomoServicePolicy<'a> {
    pub service_directory: &'a Path,
    pub controller_port: u16,
    pub controller_secret: &'a str,
}

#[derive(Debug, Eq, PartialEq)]
pub struct SanitizedMihomo {
    pub yaml: Vec<u8>,
    /// The single loopback mixed port of a proxy-mode generation; `None` for a TUN generation,
    /// which opens no inbound port.
    pub mixed_port: Option<u16>,
}

/// Top-level Mihomo keys a provider config may keep (pinned Mihomo v1.19.30). Everything else is
/// dropped: listeners and their options (`port`, `socks-port`, `redir-port`, `tproxy-port`,
/// `listeners`, `tunnels`, `*-config`, `tuic-server`, `authentication`, `lan-*`), controller and
/// UI settings (`external-*`, `secret`, `tls`; the service writes its own controller and the
/// daemon serves the dashboard, amendment A4), privileged host setup (`iptables`, `ebpf`) and
/// anything a later Mihomo adds. `mixed-port` is kept only for a proxy-mode generation, and
/// `external-controller`, `secret`, `bind-address` and `allow-lan` are always forced.
///
/// Geodata keys stay: downloads land in the client's own Mihomo home (the service keeps one
/// per user), so a user can only steer their own routing.
pub const ALLOWED_MIHOMO_KEYS: &[&str] = &[
    "mode",
    "log-level",
    "ipv6",
    "unified-delay",
    "tcp-concurrent",
    "find-process-mode",
    "global-client-fingerprint",
    "global-ua",
    "etag-support",
    "keep-alive-idle",
    "keep-alive-interval",
    "disable-keep-alive",
    "interface-name",
    "routing-mark",
    "geodata-mode",
    "geodata-loader",
    "geosite-matcher",
    "geo-auto-update",
    "geo-update-interval",
    "geox-url",
    "hosts",
    "dns",
    "ntp",
    "tun",
    "sniffer",
    "profile",
    "proxies",
    "proxy-groups",
    "proxy-providers",
    "rule-providers",
    "rules",
    "sub-rules",
    "mixed-port",
];

/// `profile` keys kept (no paths, no shared state beyond the client's own home).
const ALLOWED_PROFILE_KEYS: &[&str] = &["store-selected", "store-fake-ip"];

fn key(name: &str) -> Value {
    Value::String(name.to_owned())
}

pub fn sanitize_mihomo(
    input: &[u8],
    policy: &MihomoServicePolicy<'_>,
) -> Result<SanitizedMihomo, SanitizeError> {
    if !policy.service_directory.is_absolute() {
        return Err(SanitizeError::ServiceDirectory);
    }
    let service_directory = policy
        .service_directory
        .to_str()
        .ok_or(SanitizeError::ServiceDirectory)?;
    let mut document: Value =
        serde_yaml_ng::from_slice(input).map_err(|_| SanitizeError::Malformed)?;
    // Aliases are already expanded by the parser; merge keys are not, and Mihomo's own decoder
    // would re-apply them and bring back every key removed below.
    document
        .apply_merge()
        .map_err(|_| SanitizeError::Malformed)?;
    reject_merge_keys_and_tags(&document)?;
    let mapping = document.as_mapping_mut().ok_or(SanitizeError::Malformed)?;
    mapping.retain(|name, _| {
        name.as_str()
            .is_some_and(|name| ALLOWED_MIHOMO_KEYS.contains(&name))
    });
    if let Some(profile) = mapping.get_mut(key("profile")) {
        match profile.as_mapping_mut() {
            Some(profile) => profile.retain(|name, _| {
                name.as_str()
                    .is_some_and(|name| ALLOWED_PROFILE_KEYS.contains(&name))
            }),
            None => *profile = Value::Mapping(Mapping::new()),
        }
    }
    // Mihomo runs as SYSTEM/root: it must never set the machine clock.
    if let Some(ntp) = mapping.get_mut(key("ntp")) {
        match ntp.as_mapping_mut() {
            Some(ntp) => {
                ntp.insert(key("write-to-system"), Value::Bool(false));
            }
            None => *ntp = Value::Mapping(Mapping::new()),
        }
    }
    mapping.insert(
        key("external-controller"),
        Value::String(format!("127.0.0.1:{}", policy.controller_port)),
    );
    mapping.insert(
        key("secret"),
        Value::String(policy.controller_secret.to_owned()),
    );
    mapping.insert(key("bind-address"), Value::String("127.0.0.1".to_owned()));
    mapping.insert(key("allow-lan"), Value::Bool(false));
    let proxy_mode = mapping
        .get(key("tun"))
        .and_then(Value::as_mapping)
        .and_then(|tun| tun.get(key("enable")))
        .and_then(Value::as_bool)
        == Some(false);
    let mixed_port = if proxy_mode {
        let port = mapping
            .get(key("mixed-port"))
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or(SanitizeError::MissingMixedPort)?;
        mapping.insert(key("mixed-port"), Value::Number(port.into()));
        Some(port)
    } else {
        mapping.remove(key("mixed-port"));
        None
    };
    // Mihomo's own DNS server is another inbound listener; TUN's dns-hijack does not need it.
    if let Some(dns) = mapping.get_mut(key("dns")).and_then(Value::as_mapping_mut) {
        dns.remove(key("listen"));
    }
    let tun = mapping
        .entry(key("tun"))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if !tun.is_mapping() {
        *tun = Value::Mapping(Mapping::new());
    }
    let tun = tun.as_mapping_mut().ok_or(SanitizeError::Malformed)?;
    tun.insert(key("enable"), Value::Bool(false));
    tun.insert(key("device"), Value::String("MultiCore".to_owned()));
    // Host routing tables/rules and inherited descriptors stay Mihomo's defaults: a user must
    // not aim the privileged TUN at, say, the main routing table (254).
    for pinned in [
        "iproute2-table-index",
        "iproute2-rule-index",
        "auto-redirect-iproute2-fallback-rule-index",
        "file-descriptor",
    ] {
        tun.remove(key(pinned));
    }
    rewrite_provider_paths(mapping, "rule-providers", "rule", service_directory)?;
    rewrite_provider_paths(mapping, "proxy-providers", "proxy", service_directory)?;
    reject_credential_files(mapping)?;
    reject_merge_keys_and_tags(&document)?;
    let yaml = serde_yaml_ng::to_string(&document)
        .map_err(|_| SanitizeError::Malformed)?
        .into_bytes();
    Ok(SanitizedMihomo { yaml, mixed_port })
}

/// Provider fields the sanitiser inspects or rewrites.
const PROVIDER_CHECKED_FIELDS: &[&str] = &["type", "path", "payload", "format"];

fn rewrite_provider_paths(
    mapping: &mut Mapping,
    section: &str,
    prefix: &str,
    service_directory: &str,
) -> Result<(), SanitizeError> {
    let Some(providers) = mapping.get_mut(key(section)) else {
        return Ok(());
    };
    let providers = providers.as_mapping_mut().ok_or(SanitizeError::Malformed)?;
    for (name, provider) in providers.iter_mut() {
        let name = name.as_str().ok_or(SanitizeError::Malformed)?;
        let provider = provider.as_mapping_mut().ok_or(SanitizeError::Malformed)?;
        // Mihomo's provider decoder falls back to a case-insensitive, `_`-for-`-` match when the
        // exact key is absent; the fields this sanitiser reads must therefore appear exactly.
        for field in provider.keys() {
            let raw = field.as_str().unwrap_or("");
            let normalised = mihomo_field_name(raw)?;
            if PROVIDER_CHECKED_FIELDS.contains(&normalised.as_str()) && raw != normalised {
                return Err(SanitizeError::Malformed);
            }
        }
        let kind = provider
            .get(key("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        match kind.as_str() {
            "file" => {
                return Err(SanitizeError::FileReference(format!(
                    "{section}.{name}.path"
                )));
            }
            "inline" => {
                provider.remove(key("path"));
            }
            _ => {
                let extension = match provider.get(key("format")).and_then(Value::as_str) {
                    Some("mrs") => "mrs",
                    Some("text") => "txt",
                    _ => "yaml",
                };
                let digest = Sha256::digest(name.as_bytes());
                let file = format!(
                    "{prefix}-{}.{extension}",
                    digest[..8]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                );
                let path = Path::new(service_directory).join("providers").join(file);
                provider.insert(
                    key("path"),
                    Value::String(
                        path.to_str()
                            .ok_or(SanitizeError::ServiceDirectory)?
                            .to_owned(),
                    ),
                );
            }
        }
    }
    Ok(())
}

/// Refuses anything the YAML emitter would write back as a merge key or a tag.
fn reject_merge_keys_and_tags(value: &Value) -> Result<(), SanitizeError> {
    match value {
        Value::Mapping(mapping) => {
            for (name, child) in mapping {
                if name.as_str() == Some("<<") || !matches!(name, Value::String(_)) {
                    return Err(SanitizeError::Malformed);
                }
                reject_merge_keys_and_tags(child)?;
            }
        }
        Value::Sequence(items) => {
            for item in items {
                reject_merge_keys_and_tags(item)?;
            }
        }
        Value::Tagged(_) => return Err(SanitizeError::Malformed),
        _ => {}
    }
    Ok(())
}

/// Proxy fields Mihomo reads from disk unless they hold the material inline: `certificate` and
/// `ca` must be inline PEM, `private-key` inline PEM or a short base64 key (WireGuard). Checked
/// for every proxy type, in nested options (`plugin-opts`, …), and in inline provider payloads.
/// Remote provider content is fetched by Mihomo itself and relies on its `SAFE_PATHS` check,
/// which is the client's own home in service mode.
fn reject_credential_files(mapping: &Mapping) -> Result<(), SanitizeError> {
    if let Some(proxies) = mapping.get(key("proxies")).and_then(Value::as_sequence) {
        for (index, proxy) in proxies.iter().enumerate() {
            reject_credential_paths_in(proxy, &format!("proxies[{index}]"))?;
        }
    }
    if let Some(providers) = mapping
        .get(key("proxy-providers"))
        .and_then(Value::as_mapping)
    {
        for (name, provider) in providers {
            let name = name.as_str().unwrap_or("");
            let payload = provider.as_mapping().and_then(|provider| {
                provider
                    .iter()
                    .find(|(field, _)| {
                        field
                            .as_str()
                            .and_then(|field| mihomo_field_name(field).ok())
                            .as_deref()
                            == Some("payload")
                    })
                    .map(|(_, value)| value)
            });
            if let Some(payload) = payload.and_then(Value::as_sequence) {
                for (index, proxy) in payload.iter().enumerate() {
                    reject_credential_paths_in(
                        proxy,
                        &format!("proxy-providers.{name}.payload[{index}]"),
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// Mihomo decodes proxies and providers with its `structure` decoder, which looks keys up
/// exactly and, with `KeyReplace`, also accepts `_` for `-` (`private_key` = `private-key`).
/// Names are therefore compared ASCII-lowercased with `_` folded to `-`, and a non-ASCII key
/// (never part of a Mihomo field name) is refused outright.
fn mihomo_field_name(name: &str) -> Result<String, SanitizeError> {
    if !name.is_ascii() {
        return Err(SanitizeError::Malformed);
    }
    Ok(name.to_ascii_lowercase().replace('_', "-"))
}

fn reject_credential_paths_in(value: &Value, at: &str) -> Result<(), SanitizeError> {
    match value {
        Value::Mapping(mapping) => {
            for (name, child) in mapping {
                let name = name.as_str().unwrap_or("");
                let here = format!("{at}.{name}");
                let field = mihomo_field_name(name)?;
                if let Some(text) = child.as_str().filter(|text| !text.trim().is_empty()) {
                    let inline = match field.as_str() {
                        "certificate" | "ca" => is_pem(text),
                        "private-key" => is_pem(text) || is_wireguard_key(text),
                        _ => true,
                    };
                    if !inline {
                        return Err(SanitizeError::FileReference(here));
                    }
                }
                reject_credential_paths_in(child, &here)?;
            }
        }
        Value::Sequence(items) => {
            for (index, item) in items.iter().enumerate() {
                reject_credential_paths_in(item, &format!("{at}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_pem(text: &str) -> bool {
    text.trim_start().starts_with("-----BEGIN")
}

/// The only non-PEM `private-key` form accepted: strict standard base64 of exactly 32 bytes
/// (a WireGuard / X25519 key). Anything path-like (`/etc/shadow`, `C:\...`, `key.pem`) fails.
fn is_wireguard_key(text: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .is_ok_and(|bytes| bytes.len() == 32)
}

type JsonMap = serde_json::Map<String, serde_json::Value>;
type Json = serde_json::Value;

fn is_named(name: &str, names: &[&str]) -> bool {
    names.iter().any(|known| name.eq_ignore_ascii_case(known))
}

/// Case-folded field lookup, as Go's `encoding/json` matches struct fields.
fn field<'a>(map: &'a JsonMap, name: &str) -> Option<&'a Json> {
    map.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

/// Keeps only the fields in `allowed` (ASCII-folded) and renames them to their canonical
/// spelling. Folded duplicates were refused before, so no two keys collide.
fn keep_known(map: &mut JsonMap, allowed: &[&str]) {
    for (name, value) in std::mem::take(map) {
        if let Some(canonical) = allowed
            .iter()
            .find(|known| name.eq_ignore_ascii_case(known))
        {
            map.insert((*canonical).to_owned(), value);
        }
    }
}

/// Like [`keep_known`], but an unknown field refuses the whole config.
fn only_known(map: &mut JsonMap, allowed: &[&str], at: &str) -> Result<(), SanitizeError> {
    if let Some(name) = map.keys().find(|name| !is_named(name, allowed)) {
        return Err(SanitizeError::ForbiddenField(format!("{at}.{name}")));
    }
    keep_known(map, allowed);
    Ok(())
}

/// Xray `log` fields kept; `access`/`error` (file outputs) and anything else are dropped.
const ALLOWED_XRAY_LOG_KEYS: &[&str] = &["loglevel", "dnslog", "maskaddress"];

/// Top-level Xray sections a provider bridge config may keep (pinned Xray v26.9.9). Everything
/// else is dropped: `api` and `metrics` (their own listeners), `stats`, `reverse`, `transport`,
/// `geodata` (scheduled downloads that rename files inside the asset directory), and anything a
/// later Xray adds. `fakedns` stays because `dns` may reference it; it is an in-memory pool.
/// `observatory`/`burstObservatory` stay for balancers; they only probe through outbounds.
pub const ALLOWED_XRAY_SECTIONS: &[&str] = &[
    "log",
    "dns",
    "routing",
    "inbounds",
    "outbounds",
    "policy",
    "observatory",
    "burstObservatory",
    "fakedns",
];

/// Inbound protocols the service runs: MultiCore's Mihomo reaches Xray only through loopback
/// SOCKS bridges (`type: socks5` proxies). `tun`, `dokodemo-door` (tproxy/redirect), `http` and
/// every server protocol are refused.
pub const ALLOWED_XRAY_INBOUND_PROTOCOLS: &[&str] = &["socks"];

/// The exact shape of an inbound: a loopback SOCKS bridge (`InboundDetourConfig`, v26.9.9).
/// Anything else refuses the config: `streamSettings` (so no TLS/REALITY `target`, no hysteria
/// `masquerade`, no transport listeners), `allocate`, and any field a later Xray adds.
pub const ALLOWED_XRAY_INBOUND_KEYS: &[&str] =
    &["tag", "listen", "port", "protocol", "settings", "sniffing"];
/// `SocksServerConfig` fields. `ip` is the UDP relay address and must be a loopback IP.
const ALLOWED_XRAY_SOCKS_SETTINGS: &[&str] =
    &["auth", "udp", "ip", "accounts", "users", "userLevel"];
const ALLOWED_XRAY_SOCKS_ACCOUNT_KEYS: &[&str] = &["user", "pass"];
const ALLOWED_XRAY_SOCKS_AUTH: &[&str] = &["noauth", "password"];
/// `SniffingConfig` fields; flags and plain matcher lists only.
const ALLOWED_XRAY_SNIFFING_KEYS: &[&str] = &[
    "enabled",
    "destOverride",
    "domainsExcluded",
    "ipsExcluded",
    "metadataOnly",
    "routeOnly",
];

/// Outbound protocols of the pinned Xray (its whole client loader list); an unknown protocol is
/// refused. `wireguard` always runs with `noKernelTun: true` (a kernel TUN as root would
/// create an interface, routes and rules and zero the host's `rp_filter`).
pub const ALLOWED_XRAY_OUTBOUND_PROTOCOLS: &[&str] = &[
    "blackhole",
    "block",
    "dns",
    "direct",
    "freedom",
    "http",
    "hysteria",
    "loopback",
    "shadowsocks",
    "socks",
    "trojan",
    "vless",
    "vmess",
    "wireguard",
];
/// `OutboundDetourConfig` fields; anything else is dropped.
const ALLOWED_XRAY_OUTBOUND_KEYS: &[&str] = &[
    "protocol",
    "sendThrough",
    "tag",
    "settings",
    "streamSettings",
    "proxySettings",
    "mux",
    "targetStrategy",
];

/// `RouterConfig` fields.
const ALLOWED_XRAY_ROUTING_KEYS: &[&str] = &["rules", "domainStrategy", "balancers"];
/// Routing rule fields of the pinned Xray (`RawFieldRule` in `infra/conf/router.go`): matchers
/// and a target only. Everything else is dropped, notably `webhook`, whose notifier POSTs to a
/// URL (or an absolute/`@` unix socket) with the core's own privileges.
pub const ALLOWED_XRAY_RULE_KEYS: &[&str] = &[
    "type",
    "ruleTag",
    "outboundTag",
    "balancerTag",
    "domain",
    "domains",
    "ip",
    "port",
    "network",
    "sourceIP",
    "source",
    "sourcePort",
    "user",
    "vlessRoute",
    "inboundTag",
    "protocol",
    "attrs",
    "localIP",
    "localPort",
    "process",
    "localOS",
];
const ALLOWED_XRAY_BALANCER_KEYS: &[&str] = &["tag", "selector", "strategy", "fallbackTag"];
const ALLOWED_XRAY_STRATEGY_KEYS: &[&str] = &["type", "settings"];

/// Keys (ASCII-folded, at any depth) whose string value, or string list items, name a network
/// endpoint the core dials, resolves, relays to or posts to. Such a value may not be a local
/// socket or file ([`is_local_endpoint`]): Xray treats an absolute path or `@name` there as a
/// unix socket (REALITY `target`/`dest`, fallbacks, webhook `url`), and a root core would reach
/// sockets its user cannot. HTTP request paths (`path`, e.g. `/ws`, xhttp and gRPC paths) are
/// deliberately not in this list.
pub const XRAY_ENDPOINT_KEYS: &[&str] = &[
    "address",
    "server",
    "servers",
    "target",
    "dest",
    "destination",
    "url",
    "host",
    "endpoint",
    "redirect",
    "sendThrough",
    "rewriteAddress",
    "probeURL",
    "connectivity",
    "stunServers",
    "resolvers",
];

/// Keys refused wherever they appear (ASCII-folded): server-side features a client bridge never
/// needs (`masquerade` file server/proxy, `fallbacks`, `decryption`, `echServerKeys`) and raw
/// socket powers of the privileged core (`customSockopt` = arbitrary `setsockopt`).
pub const XRAY_FORBIDDEN_KEYS: &[&str] = &[
    "masquerade",
    "fallbacks",
    "decryption",
    "echServerKeys",
    "customSockopt",
];

pub fn sanitize_xray(input: &[u8]) -> Result<Vec<u8>, SanitizeError> {
    let mut document: Json = serde_json::from_slice(input).map_err(|_| SanitizeError::Malformed)?;
    reject_ambiguous_keys(&document)?;
    let root = document.as_object_mut().ok_or(SanitizeError::Malformed)?;
    root.retain(|name, _| is_named(name, ALLOWED_XRAY_SECTIONS));
    for (name, value) in root.iter_mut() {
        if name.eq_ignore_ascii_case("log") {
            let log = value.as_object_mut().ok_or(SanitizeError::Malformed)?;
            log.retain(|key, _| is_named(key, ALLOWED_XRAY_LOG_KEYS));
        }
    }
    reject_xray_references(&document, "$")?;
    strip_output_files(&mut document);
    let root = document.as_object_mut().ok_or(SanitizeError::Malformed)?;
    for (name, value) in root.iter_mut() {
        if name.eq_ignore_ascii_case("inbounds") {
            shape_inbounds(value)?;
        } else if name.eq_ignore_ascii_case("outbounds") {
            shape_outbounds(value)?;
        } else if name.eq_ignore_ascii_case("routing") {
            shape_routing(value)?;
        }
    }
    serde_json::to_vec(&document).map_err(|_| SanitizeError::Malformed)
}

fn is_loopback_ip(value: &Json) -> bool {
    value
        .as_str()
        .and_then(|text| text.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback())
}

fn shape_inbounds(value: &mut Json) -> Result<(), SanitizeError> {
    let inbounds = value.as_array_mut().ok_or(SanitizeError::Malformed)?;
    for (index, inbound) in inbounds.iter_mut().enumerate() {
        let at = format!("inbounds[{index}]");
        let inbound = inbound.as_object_mut().ok_or(SanitizeError::Malformed)?;
        match field(inbound, "listen") {
            None => {
                inbound.insert("listen".to_owned(), "127.0.0.1".into());
            }
            Some(listen) if is_loopback_ip(listen) => {}
            Some(_) => return Err(SanitizeError::NonLoopbackListener(format!("{at}.listen"))),
        }
        // Xray lower-cases the protocol name before looking up its loader.
        let allowed = field(inbound, "protocol")
            .and_then(Json::as_str)
            .is_some_and(|protocol| is_named(protocol, ALLOWED_XRAY_INBOUND_PROTOCOLS));
        if !allowed {
            return Err(SanitizeError::ForbiddenProtocol(format!("{at}.protocol")));
        }
        only_known(inbound, ALLOWED_XRAY_INBOUND_KEYS, &at)?;
        inbound.insert("protocol".to_owned(), "socks".into());
        // One fixed port, as a number (the service's readiness check reads it as one).
        let port = match inbound.get("port") {
            Some(Json::Number(number)) => number.as_u64(),
            Some(Json::String(text)) if text.bytes().all(|byte| byte.is_ascii_digit()) => {
                text.parse().ok()
            }
            _ => None,
        }
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| SanitizeError::ForbiddenField(format!("{at}.port")))?;
        inbound.insert("port".to_owned(), port.into());
        if inbound.get("tag").is_some_and(|tag| !tag.is_string()) {
            return Err(SanitizeError::Malformed);
        }
        if let Some(settings) = inbound.get_mut("settings") {
            shape_socks_settings(settings, &format!("{at}.settings"))?;
        }
        if let Some(sniffing) = inbound.get_mut("sniffing") {
            let sniffing = sniffing.as_object_mut().ok_or(SanitizeError::Malformed)?;
            // Unknown sniffing flags are harmless client knobs: dropped, not refused.
            keep_known(sniffing, ALLOWED_XRAY_SNIFFING_KEYS);
            for (name, value) in sniffing.iter_mut() {
                // Xray's `StringList` also takes one comma-separated string.
                if let Json::String(text) = value {
                    *value = Json::Array(
                        text.split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                            .map(|item| Json::String(item.to_owned()))
                            .collect(),
                    );
                }
                let fits = match name.as_str() {
                    "enabled" | "metadataOnly" | "routeOnly" => value.is_boolean(),
                    _ => value
                        .as_array()
                        .is_some_and(|items| items.iter().all(Json::is_string)),
                };
                if !fits {
                    return Err(SanitizeError::Malformed);
                }
            }
        }
    }
    Ok(())
}

fn shape_socks_settings(settings: &mut Json, at: &str) -> Result<(), SanitizeError> {
    if settings.is_null() {
        return Ok(());
    }
    let settings = settings.as_object_mut().ok_or(SanitizeError::Malformed)?;
    // Unknown SOCKS server keys (e.g. v2rayN's `allowTransparent`) are dropped, not refused;
    // server-side features are refused anywhere by `reject_xray_references`.
    keep_known(settings, ALLOWED_XRAY_SOCKS_SETTINGS);
    for (name, value) in settings.iter_mut() {
        match name.as_str() {
            "auth" => {
                if !value
                    .as_str()
                    .is_some_and(|auth| ALLOWED_XRAY_SOCKS_AUTH.contains(&auth))
                {
                    return Err(SanitizeError::ForbiddenField(format!("{at}.auth")));
                }
            }
            "udp" if !value.is_boolean() => return Err(SanitizeError::Malformed),
            "userLevel" if !value.is_u64() => return Err(SanitizeError::Malformed),
            "ip" if !is_loopback_ip(value) => {
                return Err(SanitizeError::NonLoopbackListener(format!("{at}.ip")));
            }
            "accounts" | "users" => {
                if value.is_null() {
                    continue;
                }
                let accounts = value.as_array_mut().ok_or(SanitizeError::Malformed)?;
                for account in accounts.iter_mut() {
                    let account = account.as_object_mut().ok_or(SanitizeError::Malformed)?;
                    keep_known(account, ALLOWED_XRAY_SOCKS_ACCOUNT_KEYS);
                    if !account.values().all(Json::is_string) {
                        return Err(SanitizeError::Malformed);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn shape_outbounds(value: &mut Json) -> Result<(), SanitizeError> {
    let outbounds = value.as_array_mut().ok_or(SanitizeError::Malformed)?;
    for (index, outbound) in outbounds.iter_mut().enumerate() {
        let outbound = outbound.as_object_mut().ok_or(SanitizeError::Malformed)?;
        keep_known(outbound, ALLOWED_XRAY_OUTBOUND_KEYS);
        let protocol = outbound
            .get("protocol")
            .and_then(Json::as_str)
            .filter(|protocol| is_named(protocol, ALLOWED_XRAY_OUTBOUND_PROTOCOLS))
            .ok_or_else(|| {
                SanitizeError::ForbiddenProtocol(format!("outbounds[{index}].protocol"))
            })?;
        if protocol.eq_ignore_ascii_case("wireguard") {
            let settings = outbound
                .entry("settings")
                .or_insert_with(|| Json::Object(JsonMap::new()));
            if settings.is_null() {
                *settings = Json::Object(JsonMap::new());
            }
            let settings = settings.as_object_mut().ok_or(SanitizeError::Malformed)?;
            settings.retain(|name, _| !name.eq_ignore_ascii_case("noKernelTun"));
            settings.insert("noKernelTun".to_owned(), true.into());
        }
    }
    Ok(())
}

fn shape_routing(value: &mut Json) -> Result<(), SanitizeError> {
    if value.is_null() {
        return Ok(());
    }
    let routing = value.as_object_mut().ok_or(SanitizeError::Malformed)?;
    keep_known(routing, ALLOWED_XRAY_ROUTING_KEYS);
    if let Some(rules) = routing.get_mut("rules").filter(|rules| !rules.is_null()) {
        for rule in rules.as_array_mut().ok_or(SanitizeError::Malformed)? {
            keep_known(
                rule.as_object_mut().ok_or(SanitizeError::Malformed)?,
                ALLOWED_XRAY_RULE_KEYS,
            );
        }
    }
    if let Some(balancers) = routing.get_mut("balancers").filter(|list| !list.is_null()) {
        for balancer in balancers.as_array_mut().ok_or(SanitizeError::Malformed)? {
            let balancer = balancer.as_object_mut().ok_or(SanitizeError::Malformed)?;
            keep_known(balancer, ALLOWED_XRAY_BALANCER_KEYS);
            if let Some(strategy) = balancer
                .get_mut("strategy")
                .filter(|value| !value.is_null())
            {
                keep_known(
                    strategy.as_object_mut().ok_or(SanitizeError::Malformed)?,
                    ALLOWED_XRAY_STRATEGY_KEYS,
                );
            }
        }
    }
    Ok(())
}

/// Xray fields that make the (privileged) core create or append to a file: TLS/REALITY
/// `masterKeyLog` (opened `O_CREATE|O_APPEND` at listener/dial setup). Removed at any depth,
/// with ASCII case folding. `log.access`/`log.error` are dropped by the `log` allowlist, and
/// the file-reading fields (`certificateFile`, `keyFile`, `ext:`) are refused below.
const XRAY_OUTPUT_FILE_KEYS: &[&str] = &["masterKeyLog"];

fn strip_output_files(value: &mut Json) {
    match value {
        Json::Object(map) => {
            map.retain(|name, _| !is_named(name, XRAY_OUTPUT_FILE_KEYS));
            for child in map.values_mut() {
                strip_output_files(child);
            }
        }
        Json::Array(items) => {
            for item in items {
                strip_output_files(item);
            }
        }
        _ => {}
    }
}

/// Go's `encoding/json` matches keys with Unicode simple case folding, so `liſten` (U+017F)
/// is `listen` and `KeyFile` with a Kelvin sign (U+212A) is `keyFile`. Every Xray field name is
/// ASCII, so any non-ASCII key anywhere is refused; the remaining ASCII keys are then compared
/// with ASCII folding, and two keys that fold together are refused (Go would pick one).
fn reject_ambiguous_keys(value: &Json) -> Result<(), SanitizeError> {
    match value {
        Json::Object(map) => {
            let mut seen = std::collections::HashSet::new();
            for (name, child) in map {
                if !name.is_ascii() || !seen.insert(name.to_ascii_lowercase()) {
                    return Err(SanitizeError::Malformed);
                }
                reject_ambiguous_keys(child)?;
            }
        }
        Json::Array(items) => {
            for item in items {
                reject_ambiguous_keys(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_external_geodata(text: &str) -> bool {
    text.get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ext:"))
}

/// A value that points at a local socket or file instead of a network host: an absolute or
/// `@abstract` unix socket path (Xray's own rule), a Windows path (drive or `\`), a `file:` or
/// `unix:`/`+unix:` URL, an `env:` indirection, or anything with a NUL.
fn is_local_endpoint(text: &str) -> bool {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let lower = trimmed.to_ascii_lowercase();
    text.contains('\0')
        || matches!(bytes.first(), Some(b'/' | b'@' | b'\\'))
        || lower.starts_with("file:")
        || lower.starts_with("env:")
        || lower.contains("unix:")
        || (bytes.len() >= 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes.get(2).is_none_or(|byte| matches!(byte, b'/' | b'\\')))
}

/// The refusals that apply at any depth of the kept sections: file reads (`certificateFile`,
/// `keyFile`, `ext:`), [`XRAY_FORBIDDEN_KEYS`], a socket `tproxy` mode, a `unix` socket `type`
/// or `network`, and a local socket or file in any [`XRAY_ENDPOINT_KEYS`] value.
fn reject_xray_references(value: &Json, at: &str) -> Result<(), SanitizeError> {
    match value {
        Json::Object(map) => {
            for (name, child) in map {
                let here = format!("{at}.{name}");
                if is_named(name, &["certificateFile", "keyFile"])
                    // `dns.hosts` keys are domain rules too.
                    || is_external_geodata(name)
                {
                    return Err(SanitizeError::FileReference(here));
                }
                if is_named(name, XRAY_FORBIDDEN_KEYS)
                    || (name.eq_ignore_ascii_case("tproxy")
                        && !child.as_str().is_some_and(|mode| {
                            mode.is_empty() || mode.eq_ignore_ascii_case("off")
                        }))
                {
                    return Err(SanitizeError::ForbiddenField(here));
                }
                if is_named(name, &["type", "network"])
                    && child
                        .as_str()
                        .is_some_and(|kind| kind.to_ascii_lowercase().contains("unix"))
                {
                    return Err(SanitizeError::LocalEndpoint(here));
                }
                if is_named(name, XRAY_ENDPOINT_KEYS) {
                    match child {
                        Json::String(text) if is_local_endpoint(text) => {
                            return Err(SanitizeError::LocalEndpoint(here));
                        }
                        Json::Array(items) => {
                            if let Some(index) = items
                                .iter()
                                .position(|item| item.as_str().is_some_and(is_local_endpoint))
                            {
                                return Err(SanitizeError::LocalEndpoint(format!(
                                    "{here}[{index}]"
                                )));
                            }
                        }
                        _ => {}
                    }
                }
                reject_xray_references(child, &here)?;
            }
        }
        Json::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                reject_xray_references(child, &format!("{at}[{index}]"))?;
            }
        }
        Json::String(text) if is_external_geodata(text) => {
            return Err(SanitizeError::FileReference(at.to_owned()));
        }
        _ => {}
    }
    Ok(())
}
