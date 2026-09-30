//! Turns a runtime generation staged by the (untrusted) user daemon into configs the privileged
//! service may run: every listener on loopback, the controller and its secret owned by the
//! service, every provider cache inside the service directory, and no file the user named.
//!
//! Listening ports (plan amendment A6): a generation staged for system-proxy mode (`tun.enable`
//! explicitly `false`, as `MihomoRuntimeControl::with_proxy_mode` writes it) keeps exactly one
//! loopback `mixed-port`; any other generation is a TUN generation and keeps no inbound port at
//! all. In proxy mode that one loopback port is reachable by every local user while it is open.

use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
};

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

/// Keys that open extra listeners, name files, run privileged network setup, or expose the
/// controller elsewhere. `external-ui*` is handled separately (prefix match; the daemon serves
/// the dashboard itself, plan amendment A4). `mixed-port` is handled by mode.
const REMOVED_MIHOMO_KEYS: &[&str] = &[
    "external-controller-unix",
    "external-controller-pipe",
    "external-controller-tls",
    "external-controller-cors",
    "external-controller-routing-mark",
    "external-doh-server",
    "tls",
    "secret",
    "listeners",
    "tunnels",
    "tuic-server",
    "ss-config",
    "vmess-config",
    "tcptun-config",
    "udptun-config",
    "iptables",
    "ebpf",
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
];

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
    let mapping = document.as_mapping_mut().ok_or(SanitizeError::Malformed)?;
    for name in REMOVED_MIHOMO_KEYS {
        mapping.remove(key(name));
    }
    let external_ui = mapping
        .keys()
        .filter(|name| {
            name.as_str()
                .is_some_and(|name| name.starts_with("external-ui"))
        })
        .cloned()
        .collect::<Vec<_>>();
    for name in external_ui {
        mapping.remove(&name);
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
    rewrite_provider_paths(mapping, "rule-providers", "rule", service_directory)?;
    rewrite_provider_paths(mapping, "proxy-providers", "proxy", service_directory)?;
    reject_credential_files(mapping)?;
    let yaml = serde_yaml_ng::to_string(&document)
        .map_err(|_| SanitizeError::Malformed)?
        .into_bytes();
    Ok(SanitizedMihomo { yaml, mixed_port })
}

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

/// Proxy fields Mihomo reads from disk when they are not inline PEM.
fn reject_credential_files(mapping: &Mapping) -> Result<(), SanitizeError> {
    let Some(proxies) = mapping.get(key("proxies")).and_then(Value::as_sequence) else {
        return Ok(());
    };
    for (index, proxy) in proxies.iter().enumerate() {
        let Some(proxy) = proxy.as_mapping() else {
            continue;
        };
        let ssh = proxy.get(key("type")).and_then(Value::as_str) == Some("ssh");
        for field in ["certificate", "private-key", "ca"] {
            if field == "private-key" && !ssh {
                // WireGuard and others carry an inline base64 key here.
                continue;
            }
            if let Some(value) = proxy.get(key(field)).and_then(Value::as_str)
                && !value.trim_start().starts_with("-----BEGIN")
            {
                return Err(SanitizeError::FileReference(format!(
                    "proxies[{index}].{field}"
                )));
            }
        }
    }
    Ok(())
}

pub fn sanitize_xray(input: &[u8]) -> Result<Vec<u8>, SanitizeError> {
    let mut document: serde_json::Value =
        serde_json::from_slice(input).map_err(|_| SanitizeError::Malformed)?;
    let root = document.as_object_mut().ok_or(SanitizeError::Malformed)?;
    if let Some(log) = root
        .get_mut("log")
        .and_then(serde_json::Value::as_object_mut)
    {
        log.remove("access");
        log.remove("error");
    }
    if let Some(inbounds) = root
        .get_mut("inbounds")
        .and_then(serde_json::Value::as_array_mut)
    {
        for (index, inbound) in inbounds.iter_mut().enumerate() {
            let inbound = inbound.as_object_mut().ok_or(SanitizeError::Malformed)?;
            match inbound.get("listen").and_then(serde_json::Value::as_str) {
                None => {
                    inbound.insert("listen".to_owned(), "127.0.0.1".into());
                }
                Some(listen)
                    if listen
                        .parse::<IpAddr>()
                        .is_ok_and(|address| address.is_loopback()) => {}
                Some(_) => {
                    return Err(SanitizeError::NonLoopbackListener(format!(
                        "inbounds[{index}].listen"
                    )));
                }
            }
        }
    }
    // Xray's gRPC API can listen by itself (`api.listen`), outside `inbounds`.
    if let Some(listen) = root
        .get("api")
        .and_then(|api| api.get("listen"))
        .and_then(serde_json::Value::as_str)
        && !listen
            .parse::<SocketAddr>()
            .is_ok_and(|address| address.ip().is_loopback())
    {
        return Err(SanitizeError::NonLoopbackListener("api.listen".to_owned()));
    }
    reject_xray_file_references(&document, "$")?;
    serde_json::to_vec(&document).map_err(|_| SanitizeError::Malformed)
}

fn reject_xray_file_references(value: &serde_json::Value, at: &str) -> Result<(), SanitizeError> {
    match value {
        serde_json::Value::Object(map) => {
            for (name, child) in map {
                let here = format!("{at}.{name}");
                if matches!(name.as_str(), "certificateFile" | "keyFile") {
                    return Err(SanitizeError::FileReference(here));
                }
                reject_xray_file_references(child, &here)?;
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                reject_xray_file_references(child, &format!("{at}[{index}]"))?;
            }
        }
        serde_json::Value::String(text) if text.starts_with("ext:") => {
            return Err(SanitizeError::FileReference(at.to_owned()));
        }
        _ => {}
    }
    Ok(())
}
