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
  - {name: wg, type: wireguard, private-key: YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXphYmNkZWY=}
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

/// Review C1: merge keys and aliases are resolved before any rule runs, so nothing removed can
/// come back through Mihomo's own merge handling, and the output has no `<<`, anchors or aliases.
#[test]
fn merge_keys_and_aliases_cannot_smuggle_removed_keys_back() {
    let top = r#"
mixed-port: 1
experimental: &smuggled {external-controller-unix: /tmp/x.sock}
tun: {enable: false}
<<: [*smuggled, {listeners: [{name: x, type: mixed, port: 7000, listen: 0.0.0.0}], external-controller-pipe: '\.\pipe\x', port: 7890, external-ui: C:\ui}]
dns: {enable: true, <<: {listen: 0.0.0.0:53}}
"#;
    let out = sanitize_mihomo(top.as_bytes(), &policy()).unwrap();
    let text = String::from_utf8(out.yaml.clone()).unwrap();
    for forbidden in [
        "<<",
        "&smuggled",
        "*smuggled",
        "listeners",
        "external-controller-unix",
        "external-controller-pipe",
        "external-ui",
        "7890",
        "0.0.0.0:53",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} survived:\n{text}");
    }
    let doc = yaml(&out.yaml);
    assert!(doc["dns"].get("listen").is_none());
    assert_eq!(doc["dns"]["enable"].as_bool(), Some(true));

    let provider = "mixed-port: 1\nrule-providers:\n  a: {<<: {type: file}, behavior: domain, path: /etc/shadow}\n";
    assert_eq!(
        sanitize_mihomo(provider.as_bytes(), &policy()),
        Err(SanitizeError::FileReference("rule-providers.a.path".into()))
    );
    let proxy = "mixed-port: 1\nproxies:\n  - {<<: {type: ssh, private-key: C:/Windows/System32/config/SAM}, name: s, server: a, port: 22}\n";
    assert_eq!(
        sanitize_mihomo(proxy.as_bytes(), &policy()),
        Err(SanitizeError::FileReference(
            "proxies[0].private-key".into()
        ))
    );
    for malformed in [
        "mixed-port: 1\n<<: 1\n",
        "mixed-port: 1\ntun: !custom {enable: true}\n",
        "mixed-port: 1\ndns: {enable: !custom true}\n",
    ] {
        assert_eq!(
            sanitize_mihomo(malformed.as_bytes(), &policy()),
            Err(SanitizeError::Malformed),
            "{malformed}"
        );
    }
}

/// Review I3: only allowlisted top-level keys survive.
#[test]
fn only_allowlisted_top_level_keys_survive() {
    let input = "mode: rule\nlog-level: info\nexperimental: {x: 1}\nauthentication: [\"u:p\"]\nfuture-listener-config: {listen: 0.0.0.0}\nskip-auth-prefixes: [0.0.0.0/0]\nprofile: {store-selected: true, cache-path: /etc/x}\nrules: [MATCH,DIRECT]\n";
    let doc = yaml(&sanitize_mihomo(input.as_bytes(), &policy()).unwrap().yaml);
    for kept in [
        "mode",
        "log-level",
        "rules",
        "profile",
        "tun",
        "external-controller",
        "secret",
    ] {
        assert!(doc.get(kept).is_some(), "{kept} must stay");
    }
    for dropped in [
        "experimental",
        "authentication",
        "future-listener-config",
        "skip-auth-prefixes",
    ] {
        assert!(doc.get(dropped).is_none(), "{dropped} must be dropped");
    }
    assert!(doc["profile"].get("cache-path").is_none());
    assert_eq!(doc["profile"]["store-selected"].as_bool(), Some(true));
    for name in doc.as_mapping().unwrap().keys() {
        let name = name.as_str().unwrap();
        assert!(
            multicore_core::ALLOWED_MIHOMO_KEYS.contains(&name)
                || ["external-controller", "secret", "bind-address", "allow-lan"].contains(&name),
            "{name}"
        );
    }
}

/// Review I2: SYSTEM/root Mihomo never writes the system clock.
#[test]
fn ntp_never_writes_the_system_clock() {
    let input = "mixed-port: 1\nntp: {enable: true, write-to-system: true, server: time.example, port: 123}\n";
    let doc = yaml(&sanitize_mihomo(input.as_bytes(), &policy()).unwrap().yaml);
    assert_eq!(doc["ntp"]["write-to-system"].as_bool(), Some(false));
    assert_eq!(doc["ntp"]["enable"].as_bool(), Some(true));
    let scalar = yaml(
        &sanitize_mihomo(b"mixed-port: 1\nntp: yes\n", &policy())
            .unwrap()
            .yaml,
    );
    assert!(scalar["ntp"].get("write-to-system").is_none());
    assert!(scalar["ntp"].as_mapping().unwrap().is_empty());
}

/// Review M6: credential paths are refused for every type, nested options and inline payloads.
#[test]
fn credential_paths_are_refused_everywhere() {
    for (input, at) in [
        (
            "mixed-port: 1\nproxies:\n  - {name: v, type: vless, server: a, port: 1, private-key: C:/k.pem, certificate: \"-----BEGIN CERTIFICATE-----\"}\n",
            "proxies[0].private-key",
        ),
        (
            "mixed-port: 1\nproxies:\n  - {name: s, type: ss, server: a, port: 1, plugin: shadow-tls, plugin-opts: {certificate: /etc/ssl/c.pem}}\n",
            "proxies[0].plugin-opts.certificate",
        ),
        (
            "mixed-port: 1\nproxy-providers:\n  p: {type: inline, payload: [{name: h, type: hysteria2, server: a, port: 1, ca: /etc/shadow}]}\n",
            "proxy-providers.p.payload[0].ca",
        ),
    ] {
        assert_eq!(
            sanitize_mihomo(input.as_bytes(), &policy()),
            Err(SanitizeError::FileReference(at.into())),
            "{input}"
        );
    }
}

/// Review C2/I1/M7: Xray matches keys case-insensitively, so the sanitiser does too.
#[test]
fn xray_keys_are_matched_case_insensitively() {
    let out = sanitize_xray(
        br#"{"LOG":{"Access":"C:/Windows/evil.txt","ERROR":"C:/e","LogLevel":"warning","dnsLog":true}}"#,
    )
    .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let log = doc["LOG"].as_object().unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log.contains_key("LogLevel") && log.contains_key("dnsLog"));
    for (input, at) in [
        (
            &br#"{"api":{"Listen":"0.0.0.0:10085","services":["HandlerService"]}}"#[..],
            "api.listen",
        ),
        (br#"{"API":{"LISTEN":"192.168.1.2:10085"}}"#, "api.listen"),
        (
            br#"{"metrics":{"tag":"m","listen":"0.0.0.0:11111"}}"#,
            "metrics.listen",
        ),
        (br#"{"Metrics":{"Listen":"[::]:11111"}}"#, "metrics.listen"),
        (
            br#"{"Inbounds":[{"Listen":"0.0.0.0","port":1}]}"#,
            "inbounds[0].listen",
        ),
    ] {
        assert_eq!(
            sanitize_xray(input),
            Err(SanitizeError::NonLoopbackListener(at.into())),
            "{}",
            String::from_utf8_lossy(input)
        );
    }
    sanitize_xray(br#"{"metrics":{"tag":"m","listen":"127.0.0.1:11111"}}"#).unwrap();
    assert_eq!(
        sanitize_xray(br#"{"outbounds":[{"streamSettings":{"tlsSettings":{"certificates":[{"CertificateFile":"C:/k.pem"}]}}}]}"#),
        Err(SanitizeError::FileReference(
            "$.outbounds[0].streamSettings.tlsSettings.certificates[0].CertificateFile".into()
        ))
    );
    assert_eq!(
        sanitize_xray(br#"{"outbounds":[{"x":{"KEYFILE":"C:/k.pem"}}]}"#),
        Err(SanitizeError::FileReference(
            "$.outbounds[0].x.KEYFILE".into()
        ))
    );
    assert_eq!(
        sanitize_xray(br#"{"dns":{"hosts":{"ext:C:/SAM:x":"1.1.1.1"}}}"#),
        Err(SanitizeError::FileReference(
            "$.dns.hosts.ext:C:/SAM:x".into()
        ))
    );
    assert_eq!(
        sanitize_xray(br#"{"routing":{"rules":[{"ip":["EXT:C:/SAM:x"]}]}}"#),
        Err(SanitizeError::FileReference(
            "$.routing.rules[0].ip[0]".into()
        ))
    );
    // Two keys that fold to the same name are ambiguous for Go: refused.
    assert_eq!(
        sanitize_xray(br#"{"inbounds":[{"listen":"127.0.0.1","Listen":"0.0.0.0"}]}"#),
        Err(SanitizeError::Malformed)
    );
    assert_eq!(
        sanitize_xray(br#"{"log":{"loglevel":"none"},"Log":{"access":"C:/x"}}"#),
        Err(SanitizeError::Malformed)
    );
}

/// Re-review C2/I1: Go folds `ſ` (U+017F) to `s` and the Kelvin sign (U+212A) to `k`, so any
/// non-ASCII Xray key is refused (confirmed live against Xray 26.9.9 by the reviewer).
#[test]
fn xray_keys_with_unicode_fold_aliases_are_refused() {
    for input in [
        "{\"api\":{\"li\u{17f}ten\":\"0.0.0.0:10085\",\"services\":[\"HandlerService\"]}}",
        "{\"metrics\":{\"li\u{17f}ten\":\"0.0.0.0:11111\"}}",
        "{\"metric\u{17f}\":{\"listen\":\"0.0.0.0:11111\"}}",
        "{\"inbound\u{17f}\":[{\"listen\":\"0.0.0.0\",\"port\":1}]}",
        "{\"inbounds\":[{\"li\u{17f}ten\":\"0.0.0.0\",\"port\":1}]}",
        "{\"outbounds\":[{\"x\":{\"\u{212a}eyFile\":\"C:/k.pem\"}}]}",
        "{\"log\":{\"acce\u{17f}\u{17f}\":\"C:/Windows/evil.txt\"}}",
        "{\"routing\":{\"rules\":[{\"é\":1}]}}",
    ] {
        assert_eq!(
            sanitize_xray(input.as_bytes()),
            Err(SanitizeError::Malformed),
            "{input}"
        );
    }
    // Non-ASCII *values* stay allowed (e.g. remarks, IDN domains).
    sanitize_xray("{\"outbounds\":[{\"tag\":\"узел\",\"protocol\":\"freedom\"}]}".as_bytes())
        .unwrap();
}

/// Mihomo's `structure` decoder accepts `_` for `-` in proxy fields, and field names are ASCII.
#[test]
fn mihomo_field_aliases_and_non_ascii_keys_are_handled() {
    let underscore = "mixed-port: 1\nproxies:\n  - {name: s, type: ssh, server: a, port: 22, private_key: /etc/shadow}\n";
    assert_eq!(
        sanitize_mihomo(underscore.as_bytes(), &policy()),
        Err(SanitizeError::FileReference(
            "proxies[0].private_key".into()
        ))
    );
    let mixed_case = "mixed-port: 1\nproxies:\n  - {name: v, type: vless, server: a, port: 1, Certificate: /etc/ssl/c.pem}\n";
    assert_eq!(
        sanitize_mihomo(mixed_case.as_bytes(), &policy()),
        Err(SanitizeError::FileReference(
            "proxies[0].Certificate".into()
        ))
    );
    for input in [
        "mixed-port: 1\nproxies:\n  - {name: s, type: ssh, server: a, port: 22, \"private-\u{212a}ey\": /etc/shadow}\n",
        "mixed-port: 1\nrule-providers:\n  a: {type: http, behavior: domain, url: 'https://x', \"pa\u{17f}h\": /etc/x}\n",
    ] {
        assert_eq!(
            sanitize_mihomo(input.as_bytes(), &policy()),
            Err(SanitizeError::Malformed),
            "{input}"
        );
    }
}

/// Re-review N1: `private-key` is PEM or a strict 32-byte base64 key, never a path.
#[test]
fn private_key_paths_are_refused_for_every_type() {
    for (kind, key) in [
        ("ssh", "/etc/shadow"),
        ("ssh", "/etc/sudoers"),
        ("wireguard", "/etc/gshadow"),
        ("wireguard", "YWJjZA=="),
        ("ssh", "C:\\Windows\\System32\\config\\SAM"),
        ("vless", "key.pem"),
        ("vless", "keys/client"),
    ] {
        let input = format!(
            "mixed-port: 1\nproxies:\n  - {{name: p, type: {kind}, server: a, port: 1, private-key: '{key}'}}\n"
        );
        assert_eq!(
            sanitize_mihomo(input.as_bytes(), &policy()),
            Err(SanitizeError::FileReference(
                "proxies[0].private-key".into()
            )),
            "{kind} {key}"
        );
    }
    let wireguard = "mixed-port: 1\nproxies:\n  - {name: w, type: wireguard, server: a, port: 1, private-key: YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXphYmNkZWY=}\n";
    assert!(sanitize_mihomo(wireguard.as_bytes(), &policy()).is_ok());
    // Empty credential fields mean "absent".
    let empty = "mixed-port: 1\nproxies:\n  - {name: h, type: hysteria2, server: a, port: 1, ca: '', certificate: '', private-key: ''}\n";
    assert!(sanitize_mihomo(empty.as_bytes(), &policy()).is_ok());
}

#[test]
fn tun_routing_table_and_descriptor_overrides_are_dropped() {
    let input = "mixed-port: 1\ntun: {enable: true, stack: system, iproute2-table-index: 254, iproute2-rule-index: 0, file-descriptor: 3}\n";
    let doc = yaml(&sanitize_mihomo(input.as_bytes(), &policy()).unwrap().yaml);
    for dropped in [
        "iproute2-table-index",
        "iproute2-rule-index",
        "file-descriptor",
    ] {
        assert!(doc["tun"].get(dropped).is_none(), "{dropped}");
    }
    assert_eq!(doc["tun"]["stack"].as_str(), Some("system"));
}
