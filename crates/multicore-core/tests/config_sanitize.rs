use std::path::Path;

use multicore_core::{
    MihomoRuntimeControl, MihomoServicePolicy, SanitizeError, Snapshot, sanitize_mihomo,
    sanitize_xray, stage_runtime_with_mihomo_control,
};
use serde_yaml_ng::Value;

#[cfg(windows)]
const SERVICE_DIR: &str = r"C:\ProgramData\MultiCore\service";
#[cfg(not(windows))]
const SERVICE_DIR: &str = "/var/lib/multicore";

fn policy() -> MihomoServicePolicy<'static> {
    MihomoServicePolicy {
        service_directory: Path::new(SERVICE_DIR),
        controller_port: 41234,
        controller_secret: "service-secret",
    }
}

fn yaml(bytes: &[u8]) -> Value {
    serde_yaml_ng::from_slice(bytes).unwrap()
}

const BASE: &str = r#"
mixed-port: 28765
port: 7890
socks-port: 7891
redir-port: 7892
tproxy-port: 7893
bind-address: '*'
allow-lan: true
external-controller: 0.0.0.0:9090
external-controller-unix: /tmp/x.sock
external-controller-pipe: \\.\pipe\x
external-ui: C:\Users\me\ui
external-ui-url: https://example.invalid/ui.zip
secret: user-secret
tls: {certificate: C:\Users\me\c.pem, private-key: C:\Users\me\k.pem}
listeners: [{name: x, type: mixed, port: 7000, listen: 0.0.0.0}]
iptables: {enable: true}
dns: {enable: true, listen: 0.0.0.0:53}
tun: {enable: true, device: Evil, stack: system, auto-route: true}
rule-providers:
  ads: {type: http, behavior: domain, url: 'https://example.invalid/a.yaml', path: C:\Windows\System32\drivers\etc\hosts}
  inline-set: {type: inline, behavior: domain, payload: [example.com], path: ../x}
proxy-providers:
  subscription: {type: http, url: 'https://example.invalid/p', path: ../../evil.yaml}
proxies:
  - {name: wg, type: wireguard, private-key: aGVsbG8gd29ybGQgaGVsbG8gd29ybGQgaGVsbG8gd28=}
"#;

/// Listener keys that A6 removes in TUN mode (and, except `mixed-port`, in proxy mode too).
const INBOUND_KEYS: [&str; 5] = [
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
    "listeners",
];

#[test]
fn controller_listeners_ui_and_tun_are_forced() {
    let out = sanitize_mihomo(BASE.as_bytes(), &policy()).unwrap();
    let doc = yaml(&out.yaml);
    assert_eq!(doc["external-controller"].as_str(), Some("127.0.0.1:41234"));
    assert_eq!(doc["secret"].as_str(), Some("service-secret"));
    assert_eq!(doc["bind-address"].as_str(), Some("127.0.0.1"));
    assert_eq!(doc["allow-lan"].as_bool(), Some(false));
    assert_eq!(doc["tun"]["enable"].as_bool(), Some(false));
    assert_eq!(doc["tun"]["device"].as_str(), Some("MultiCore"));
    assert_eq!(doc["tun"]["auto-route"].as_bool(), Some(true));
    assert!(doc["dns"].get("listen").is_none(), "no DNS listener");
    assert_eq!(doc["dns"]["enable"].as_bool(), Some(true));
    for removed in [
        "external-controller-unix",
        "external-controller-pipe",
        "external-ui",
        "external-ui-url",
        "tls",
        "listeners",
        "iptables",
    ] {
        assert!(doc.get(removed).is_none(), "{removed} must be removed");
    }
}

#[test]
fn tun_mode_opens_no_inbound_port() {
    // A generation staged for TUN (tun not explicitly disabled) keeps no listener at all.
    let out = sanitize_mihomo(BASE.as_bytes(), &policy()).unwrap();
    assert_eq!(out.mixed_port, None);
    let doc = yaml(&out.yaml);
    assert!(doc.get("mixed-port").is_none());
    for removed in INBOUND_KEYS {
        assert!(doc.get(removed).is_none(), "{removed} must be removed");
    }
    let bare = sanitize_mihomo(b"tun: {enable: true}\n", &policy()).unwrap();
    assert_eq!(bare.mixed_port, None);
    let no_tun_section = sanitize_mihomo(b"mixed-port: 0\n", &policy()).unwrap();
    assert_eq!(no_tun_section.mixed_port, None);
    assert!(yaml(&no_tun_section.yaml).get("mixed-port").is_none());
}

#[test]
fn proxy_mode_opens_exactly_one_loopback_mixed_port() {
    let proxy_form = BASE.replace(
        "tun: {enable: true, device: Evil",
        "tun: {enable: false, device: Evil",
    );
    let out = sanitize_mihomo(proxy_form.as_bytes(), &policy()).unwrap();
    assert_eq!(out.mixed_port, Some(28765));
    let doc = yaml(&out.yaml);
    assert_eq!(doc["mixed-port"].as_u64(), Some(28765));
    assert_eq!(doc["bind-address"].as_str(), Some("127.0.0.1"));
    assert_eq!(doc["allow-lan"].as_bool(), Some(false));
    assert_eq!(doc["tun"]["enable"].as_bool(), Some(false));
    for removed in INBOUND_KEYS {
        assert!(doc.get(removed).is_none(), "{removed} must be removed");
    }
}

#[test]
fn provider_paths_are_rewritten_into_the_service_directory_by_name() {
    let out = sanitize_mihomo(BASE.as_bytes(), &policy()).unwrap();
    let doc = yaml(&out.yaml);
    let rule = doc["rule-providers"]["ads"]["path"].as_str().unwrap();
    let proxy = doc["proxy-providers"]["subscription"]["path"]
        .as_str()
        .unwrap();
    for path in [rule, proxy] {
        assert!(
            Path::new(path).starts_with(Path::new(SERVICE_DIR).join("providers")),
            "{path}"
        );
        assert!(!path.contains(".."));
    }
    assert!(
        Path::new(rule)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("rule-")
    );
    assert!(
        Path::new(proxy)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("proxy-")
    );
    assert!(doc["rule-providers"]["inline-set"].get("path").is_none());
    let again = sanitize_mihomo(BASE.as_bytes(), &policy()).unwrap();
    assert_eq!(out.yaml, again.yaml, "names map to stable paths");
}

#[test]
fn local_file_references_are_refused() {
    let file_provider = "mixed-port: 1\nrule-providers:\n  local: {type: file, behavior: domain, path: C:\\Users\\me\\r.yaml}\n";
    assert_eq!(
        sanitize_mihomo(file_provider.as_bytes(), &policy()),
        Err(SanitizeError::FileReference(
            "rule-providers.local.path".into()
        ))
    );
    let ssh_path = "mixed-port: 1\nproxies:\n  - {name: s, type: ssh, server: a, port: 22, private-key: C:\\Windows\\System32\\config\\SAM}\n";
    assert_eq!(
        sanitize_mihomo(ssh_path.as_bytes(), &policy()),
        Err(SanitizeError::FileReference(
            "proxies[0].private-key".into()
        ))
    );
    let ssh_inline = "mixed-port: 1\nproxies:\n  - {name: s, type: ssh, server: a, port: 22, private-key: \"-----BEGIN OPENSSH PRIVATE KEY-----\\nAAAA\\n-----END OPENSSH PRIVATE KEY-----\"}\n";
    assert!(sanitize_mihomo(ssh_inline.as_bytes(), &policy()).is_ok());
    let ca_path = "mixed-port: 1\nproxies:\n  - {name: h, type: hysteria2, server: a, port: 1, ca: /etc/shadow}\n";
    assert_eq!(
        sanitize_mihomo(ca_path.as_bytes(), &policy()),
        Err(SanitizeError::FileReference("proxies[0].ca".into()))
    );
}

#[test]
fn a_proxy_mode_config_without_a_mixed_port_is_refused() {
    assert_eq!(
        sanitize_mihomo(b"tun: {enable: false}\n", &policy()),
        Err(SanitizeError::MissingMixedPort)
    );
    assert_eq!(
        sanitize_mihomo(b"mixed-port: 0\ntun: {enable: false}\n", &policy()),
        Err(SanitizeError::MissingMixedPort)
    );
    assert_eq!(
        sanitize_mihomo(b"mixed-port: 70000\ntun: {enable: false}\n", &policy()),
        Err(SanitizeError::MissingMixedPort)
    );
    assert_eq!(
        sanitize_mihomo(b"- a\n- b\n", &policy()),
        Err(SanitizeError::Malformed)
    );
}

#[test]
fn a_relative_service_directory_is_refused() {
    let relative = MihomoServicePolicy {
        service_directory: Path::new("service"),
        ..policy()
    };
    assert_eq!(
        sanitize_mihomo(b"mixed-port: 1\n", &relative),
        Err(SanitizeError::ServiceDirectory)
    );
}

#[test]
fn xray_logs_listeners_and_file_references_are_sanitised() {
    let out = sanitize_xray(
        br#"{"log":{"loglevel":"warning","access":"C:\\Windows\\a.log","error":"/etc/x"},
            "inbounds":[{"port":1080,"protocol":"socks"},{"listen":"127.0.0.1","port":1081,"protocol":"socks"}],
            "outbounds":[{"protocol":"freedom"}]}"#,
    )
    .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(doc["log"].get("access").is_none());
    assert!(doc["log"].get("error").is_none());
    assert_eq!(doc["log"]["loglevel"], "warning");
    assert_eq!(doc["inbounds"][0]["listen"], "127.0.0.1");
    assert_eq!(
        sanitize_xray(br#"{"inbounds":[{"listen":"0.0.0.0","port":1}]}"#),
        Err(SanitizeError::NonLoopbackListener(
            "inbounds[0].listen".into()
        ))
    );
    assert_eq!(
        sanitize_xray(br#"{"inbounds":[{"listen":"/run/x.sock","port":1}]}"#),
        Err(SanitizeError::NonLoopbackListener(
            "inbounds[0].listen".into()
        ))
    );
    assert_eq!(
        sanitize_xray(br#"{"api":{"tag":"api","listen":"0.0.0.0:10085"}}"#),
        Err(SanitizeError::NonLoopbackListener("api.listen".into()))
    );
    sanitize_xray(br#"{"api":{"tag":"api","listen":"127.0.0.1:10085"}}"#).unwrap();
    assert_eq!(
        sanitize_xray(
            br#"{"outbounds":[{"streamSettings":{"tlsSettings":{"certificates":[{"certificateFile":"C:\\k.pem"}]}}}]}"#
        ),
        Err(SanitizeError::FileReference(
            "$.outbounds[0].streamSettings.tlsSettings.certificates[0].certificateFile".into()
        ))
    );
    assert_eq!(
        sanitize_xray(br#"{"routing":{"rules":[{"domain":["ext:..\\..\\SAM:x"]}]}}"#),
        Err(SanitizeError::FileReference(
            "$.routing.rules[0].domain[0]".into()
        ))
    );
    assert_eq!(sanitize_xray(b"[1]"), Err(SanitizeError::Malformed));
}

#[test]
fn a_daemon_staged_proxy_mode_generation_is_accepted() {
    let snapshot = Snapshot::parse(
        b"proxies:\n  - {name: a, type: socks5, server: 127.0.0.1, port: 1}\nproxy-groups:\n  - {name: g, type: select, proxies: [a]}\nrules: [MATCH,g]\n",
        multicore_core::IDLE_XRAY_CONFIG,
    )
    .unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "daemon-secret")
        .unwrap()
        .with_proxy_mode(28765)
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let staged = stage_runtime_with_mihomo_control(&snapshot, root.path(), &control).unwrap();
    let mihomo = std::fs::read(&staged.paths().mihomo_config).unwrap();
    let xray = std::fs::read(&staged.paths().xray_config).unwrap();
    let out = sanitize_mihomo(&mihomo, &policy()).unwrap();
    assert_eq!(out.mixed_port, Some(28765));
    assert!(
        !String::from_utf8(out.yaml)
            .unwrap()
            .contains("daemon-secret")
    );
    sanitize_xray(&xray).unwrap();
}

#[test]
fn a_daemon_staged_tun_mode_generation_is_accepted_without_ports() {
    let snapshot = Snapshot::parse(
        b"mixed-port: 7890\nproxies:\n  - {name: a, type: socks5, server: 127.0.0.1, port: 1}\nproxy-groups:\n  - {name: g, type: select, proxies: [a]}\nrules: [MATCH,g]\n",
        multicore_core::IDLE_XRAY_CONFIG,
    )
    .unwrap();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "daemon-secret").unwrap();
    let root = tempfile::tempdir().unwrap();
    let staged = stage_runtime_with_mihomo_control(&snapshot, root.path(), &control).unwrap();
    let mihomo = std::fs::read(&staged.paths().mihomo_config).unwrap();
    let out = sanitize_mihomo(&mihomo, &policy()).unwrap();
    assert_eq!(out.mixed_port, None);
    let doc = yaml(&out.yaml);
    assert!(doc.get("mixed-port").is_none());
    assert_eq!(doc["tun"]["enable"].as_bool(), Some(false));
}
