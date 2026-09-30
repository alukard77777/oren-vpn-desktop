#[cfg(windows)]
use std::process::{Command, Stdio};
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use multicore_core::{
    MihomoRuntimeControl, PersistentSnapshotStore, Snapshot, publish_runtime_with_mihomo_control,
    stage_runtime_with_mihomo_control, xray_outbound_server_domains,
};
use serde_json::Value as JsonValue;
use serde_yaml_ng::Value;

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn live_dns_uses_only_yandex_for_endpoints_without_static_ip_pinning() {
    let xray = br#"{
      "dns": {"servers": ["8.8.8.8"], "hosts": {"internal.example": "192.0.2.9"}},
      "outbounds": [{"tag":"vpn", "protocol":"shadowsocks",
        "settings":{"address":"edge.example.com"},
        "streamSettings":{"finalmask":{"enabled":true}}}],
      "routing":{"rules":[{"type":"field","network":"tcp,udp","outboundTag":"vpn"}]}
    }"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: JsonValue =
        serde_json::from_slice(&fs::read(published.xray_config).unwrap()).unwrap();
    assert_eq!(
        runtime["dns"]["servers"][1],
        serde_json::json!({
            "address":"77.88.8.8", "domains":["full:edge.example.com"],
            "skipFallback":true, "finalQuery":true, "disableCache":false,
            "serveStale":false, "tag":"multicore-endpoint-dns", "timeoutMs":1200
        })
    );
    assert_eq!(runtime["dns"]["servers"][2], "8.8.8.8");
    assert!(runtime["dns"]["hosts"].get("edge.example.com").is_none());
    assert_eq!(runtime["dns"]["hosts"]["internal.example"], "192.0.2.9");
    assert_eq!(runtime["outbounds"][0]["tag"], "vpn");
    assert_eq!(
        runtime["outbounds"][0]["streamSettings"]["sockopt"]["domainStrategy"],
        "ForceIP"
    );
    assert_eq!(
        runtime["outbounds"][0]["streamSettings"]["finalmask"]["enabled"],
        true
    );
    assert_eq!(
        runtime["outbounds"][1],
        serde_json::json!({
            "tag":"multicore-endpoint-dns-direct", "protocol":"freedom",
            "settings":{"domainStrategy":"AsIs"},
            "streamSettings":{"sockopt":{"domainStrategy":"AsIs"}}
        }),
        "the endpoint-DNS-direct outbound must never bind to the physical \
         interface - Mihomo's DNS-leak protection blackholes UDP:53 \
         explicitly bound to it, proven on real hardware"
    );
    assert_eq!(
        runtime["routing"]["rules"][0],
        serde_json::json!({
            "type":"field", "inboundTag":["multicore-endpoint-dns"],
            "outboundTag":"multicore-endpoint-dns-direct"
        })
    );
    assert_eq!(runtime["routing"]["rules"][1]["outboundTag"], "vpn");
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[test]
fn mihomo_nameserver_policy_routes_live_endpoint_domains_directly_around_the_dns_hijack() {
    // Mihomo's TUN device hijacks every outbound port-53 packet on the system
    // (`dns-hijack: [any:53]`) regardless of source process or destination
    // address, so Xray's own scoped resolver query for an endpoint domain
    // never actually reaches 77.88.8.8: it gets redirected straight back
    // into Mihomo, which then tries to resolve it through its normal
    // proxy-routed nameservers - which depend on the very proxy connection
    // that resolving this domain is needed to establish. A live capture on
    // real hardware reproduced this exact deadlock: the endpoint domain
    // never resolved, indefinitely, once the TUN adapter came up, even
    // though the same resolver answered fine for every other domain.
    // `nameserver-policy` must route it directly, breaking the cycle.
    let mihomo = br#"
proxies: []
tun:
  enable: true
  dns-hijack: [any:53]
dns:
  enhanced-mode: fake-ip
  nameserver-policy:
    'other.example': 1.1.1.1
"#;
    let xray = br#"{"outbounds": [{"tag":"vpn","protocol":"shadowsocks",
        "settings":{"address":"se.acmevpn.net","port":443}}]}"#;
    let snapshot = Snapshot::parse(mihomo, xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let policy = runtime
        .as_mapping()
        .unwrap()
        .get(Value::String("dns".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap()
        .get(Value::String("nameserver-policy".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap();

    assert_eq!(
        policy.get(Value::String("se.acmevpn.net".to_owned())),
        Some(&Value::String("77.88.8.8".to_owned()))
    );
    // The provider's own unrelated policy entry survives the merge.
    assert_eq!(
        policy.get(Value::String("other.example".to_owned())),
        Some(&Value::String("1.1.1.1".to_owned()))
    );
}

#[test]
fn mihomo_nameserver_policy_is_untouched_without_a_live_endpoint_domain() {
    let mihomo = br#"
proxies: []
dns:
  enhanced-mode: fake-ip
"#;
    // No live-DNS interface configured at all.
    let xray = br#"{"outbounds": [{"tag":"vpn","protocol":"shadowsocks",
        "settings":{"address":"se.acmevpn.net","port":443}}]}"#;
    let snapshot = Snapshot::parse(mihomo, xray).unwrap();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    let directory = TempDirectory::new();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let dns = runtime
        .as_mapping()
        .unwrap()
        .get(Value::String("dns".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap();
    assert!(
        dns.get(Value::String("nameserver-policy".to_owned()))
            .is_none()
    );

    // Live-DNS interface configured, but every outbound already dials a literal IP.
    let literal_xray = br#"{"outbounds": [{"tag":"vpn","protocol":"shadowsocks",
        "settings":{"address":"192.0.2.10","port":443}}]}"#;
    let snapshot = Snapshot::parse(mihomo, literal_xray).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();
    let directory = TempDirectory::new();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let dns = runtime
        .as_mapping()
        .unwrap()
        .get(Value::String("dns".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap();
    assert!(
        dns.get(Value::String("nameserver-policy".to_owned()))
            .is_none()
    );
}

#[test]
fn runtime_overlay_owns_a_unique_tun_device() {
    let mihomo = br#"
proxies: []
tun:
  enable: true
  device: FlClashX
  stack: system
  auto-route: true
  strict-route: true
  dns-hijack: [any:53]
"#;
    let snapshot = Snapshot::parse(mihomo, br#"{}"#).unwrap();
    let directory = TempDirectory::new();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let tun = runtime
        .as_mapping()
        .unwrap()
        .get(Value::String("tun".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap();

    assert_eq!(
        tun.get(Value::String("device".to_owned()))
            .and_then(Value::as_str),
        Some("MultiCore")
    );
    assert_eq!(
        tun.get(Value::String("stack".to_owned()))
            .and_then(Value::as_str),
        Some("system")
    );
    assert_eq!(
        tun.get(Value::String("auto-route".to_owned()))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(snapshot.mihomo.raw_yaml().as_bytes(), mihomo);
}

#[test]
fn proxy_mode_disables_tun_and_listens_on_loopback_only() {
    let mihomo = br#"
proxies: []
allow-lan: true
bind-address: "*"
mixed-port: 7890
tun:
  enable: true
  device: FlClashX
  stack: system
"#;
    let snapshot = Snapshot::parse(mihomo, br#"{}"#).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_proxy_mode(28765)
        .unwrap();
    assert_eq!(control.proxy_mixed_port(), Some(28765));

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let mapping = runtime.as_mapping().unwrap();
    let key = |name: &str| Value::String(name.to_owned());
    let tun = mapping.get(key("tun")).and_then(Value::as_mapping).unwrap();

    assert_eq!(tun.get(key("enable")).and_then(Value::as_bool), Some(false));
    assert_eq!(
        tun.get(key("device")).and_then(Value::as_str),
        Some("MultiCore")
    );
    assert_eq!(
        mapping.get(key("mixed-port")).and_then(Value::as_u64),
        Some(28765)
    );
    assert_eq!(
        mapping.get(key("bind-address")).and_then(Value::as_str),
        Some("127.0.0.1")
    );
    assert_eq!(
        mapping.get(key("allow-lan")).and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn proxy_mode_requires_a_concrete_port() {
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    assert!(control.with_proxy_mode(0).is_err());
}

#[test]
fn tun_mode_leaves_provider_listeners_untouched() {
    let snapshot = Snapshot::parse(
        b"proxies: []
allow-lan: true
",
        br#"{}"#,
    )
    .unwrap();
    let directory = TempDirectory::new();
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let mapping = runtime.as_mapping().unwrap();
    assert_eq!(
        mapping
            .get(Value::String("allow-lan".to_owned()))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert!(
        mapping
            .get(Value::String("mixed-port".to_owned()))
            .is_none()
    );
    let tun = mapping
        .get(Value::String("tun".to_owned()))
        .and_then(Value::as_mapping)
        .unwrap();
    assert!(tun.get(Value::String("enable".to_owned())).is_none());
}

fn live_dns_overlay(document: JsonValue) -> Result<JsonValue, multicore_core::PersistenceError> {
    let bytes = serde_json::to_vec(&document).unwrap();
    let snapshot = Snapshot::parse(b"proxies: []\n", &bytes).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();
    let published = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control)?;
    Ok(serde_json::from_slice(&fs::read(published.xray_config).unwrap()).unwrap())
}

#[test]
fn live_dns_rejects_reserved_tag_collisions_and_malformed_policy() {
    for patch in [
        serde_json::json!({"inbounds":[{"tag":"multicore-endpoint-dns"}]}),
        serde_json::json!({"dns":{"tag":"multicore-endpoint-dns-direct"}}),
        serde_json::json!({"routing":{"rules":[{"outboundTag":"multicore-endpoint-dns-direct"}]}}),
        serde_json::json!({"dns":{"servers":"invalid"}}),
        serde_json::json!({"routing":{"rules":{}}}),
        serde_json::json!({"dns":[]}),
        serde_json::json!({"routing":[]}),
    ] {
        let mut input =
            serde_json::json!({"outbounds":[{"settings":{"address":"edge.example.com"}}]});
        input
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(live_dns_overlay(input.clone()).is_err(), "accepted {input}");
    }
}

#[test]
fn live_dns_keeps_provider_dns_policy_and_handles_absent_or_null_sections() {
    for optional in [
        serde_json::json!({}),
        serde_json::json!({"dns":null,"routing":null}),
    ] {
        let mut input =
            serde_json::json!({"outbounds":[{"settings":{"address":"edge.example.com"}}]});
        input
            .as_object_mut()
            .unwrap()
            .extend(optional.as_object().unwrap().clone());
        let runtime = live_dns_overlay(input).unwrap();
        assert!(runtime["dns"].get("hosts").is_none());
        assert_eq!(runtime["dns"]["servers"][1]["address"], "77.88.8.8");
        assert_eq!(runtime["dns"]["servers"][2], "localhost");
        assert_eq!(runtime["routing"]["rules"].as_array().unwrap().len(), 1);
    }
    let runtime = live_dns_overlay(serde_json::json!({
        "outbounds":[{"settings":{"address":"edge.example.com"}}],
        "dns":{"hosts":{"fixed.example":"192.0.2.3"}, "tag":"provider-dns",
          "disableCache":true,"serveStale":true,"queryStrategy":"UseIPv4",
          "enableParallelQuery":true,"servers":[{"address":"1.1.1.1","domains":["domain:example"]}]},
        "routing":{"domainStrategy":"IPIfNonMatch","rules":null}
    })).unwrap();
    assert_eq!(runtime["dns"]["tag"], "provider-dns");
    assert_eq!(runtime["dns"]["disableCache"], true);
    assert_eq!(runtime["dns"]["serveStale"], true);
    assert_eq!(runtime["dns"]["queryStrategy"], "UseIPv4");
    assert_eq!(runtime["dns"]["servers"][1]["disableCache"], false);
    assert_eq!(runtime["dns"]["servers"][1]["serveStale"], false);
    assert_eq!(runtime["routing"]["domainStrategy"], "IPIfNonMatch");
}

#[test]
fn live_dns_does_not_add_a_resolver_for_ip_only_endpoints() {
    let input = serde_json::json!({
        "outbounds":[{"settings":{"address":"192.0.2.10"}}],
        "dns":{"servers":["1.1.1.1"]},"routing":{"rules":[]}
    });
    let runtime = live_dns_overlay(input.clone()).unwrap();
    assert_eq!(runtime["dns"], input["dns"]);
    assert_eq!(runtime["routing"], input["routing"]);
    assert_eq!(runtime["outbounds"].as_array().unwrap().len(), 1);
}

#[test]
fn live_dns_preserves_the_provider_emergency_resolver() {
    let original = serde_json::json!({"address":"192.0.2.53", "port":5353,
        "tag":"provider-dns", "domains":["domain:private.example"],
        "skipFallback":true,"queryStrategy":"UseIPv4"});
    let runtime = live_dns_overlay(serde_json::json!({
        "outbounds":[{"settings":{"address":"edge.example.com"}}],
        "dns":{"disableFallback":true,"servers":[original.clone()]}
    }))
    .unwrap();
    let mut guard = original.clone();
    guard["domains"] = serde_json::json!([]);
    assert_eq!(
        runtime["dns"]["servers"][0], guard,
        "Xray unconditionally uses server 0 when no match/fallback exists"
    );
    assert_eq!(runtime["dns"]["servers"][1]["address"], "77.88.8.8");
    assert_eq!(runtime["dns"]["servers"][2], original);
    let empty = live_dns_overlay(serde_json::json!({
        "outbounds":[{"settings":{"address":"edge.example.com"}}],"dns":{"servers":[]}
    }))
    .unwrap();
    assert_eq!(empty["dns"]["servers"][0]["address"], "localhost");
    assert_eq!(empty["dns"]["servers"][2], "localhost");
}

#[test]
fn live_dns_rejects_balancer_and_observatory_prefixes_that_capture_the_bypass() {
    for selector in ["", "multicore-", "proxy,multicore-"] {
        assert!(
            live_dns_overlay(serde_json::json!({
                "outbounds":[{"settings":{"address":"edge.example.com"}}],
                "routing":{"balancers":[{"tag":"balance","selector":selector}]}
            }))
            .is_err(),
            "scalar selector captures bypass: {selector}"
        );
    }
    for selector in ["", "multi", "multicore-", "multicore-endpoint-dns-direct"] {
        for section in [
            serde_json::json!({"routing":{"balancers":[{"tag":"balance","selector":[selector]}]}}),
            serde_json::json!({"observatory":{"subjectSelector":[selector]}}),
            serde_json::json!({"burstObservatory":{"subjectSelector":[selector]}}),
        ] {
            let mut input = serde_json::json!({"outbounds":[{"tag":"proxy","settings":{"address":"edge.example.com"}}]});
            input
                .as_object_mut()
                .unwrap()
                .extend(section.as_object().unwrap().clone());
            assert!(
                live_dns_overlay(input.clone()).is_err(),
                "bypass captured by {input}"
            );
        }
    }
    assert!(
        live_dns_overlay(serde_json::json!({
            "outbounds":[{"tag":"proxy","settings":{"address":"edge.example.com"}}],
            "routing":{"balancers":[{"tag":"balance","selector":["proxy"]}]}
        }))
        .is_ok()
    );
}

#[test]
fn live_dns_normalizes_fqdn_matching_without_rewriting_the_endpoint() {
    let runtime = live_dns_overlay(serde_json::json!({
        "outbounds":[{"settings":{"address":"EDGE.Example.COM."}}]
    }))
    .unwrap();
    assert_eq!(
        runtime["dns"]["servers"][1]["domains"],
        serde_json::json!(["full:edge.example.com"])
    );
    assert_eq!(
        runtime["outbounds"][0]["settings"]["address"],
        "EDGE.Example.COM."
    );
}

#[test]
fn live_dns_matches_xray_whitespace_normalization() {
    let runtime = live_dns_overlay(serde_json::json!({
        "outbounds":[{"settings":{"address":" EDGE.Example.COM. "}}]
    }))
    .unwrap();
    assert_eq!(
        runtime["dns"]["servers"][1]["domains"],
        serde_json::json!(["full:edge.example.com"])
    );
    assert_eq!(
        runtime["outbounds"][0]["settings"]["address"],
        " EDGE.Example.COM. "
    );
    assert!(
        live_dns_overlay(serde_json::json!({
            "outbounds":[{"settings":{"address":"edge.local"}}],
            "dns":{"servers":[" localhost "]}
        }))
        .is_err()
    );
}

#[test]
fn live_dns_supports_vless_and_vmess_vnext_endpoint_shapes() {
    let runtime = live_dns_overlay(serde_json::json!({"outbounds":[
        {"protocol":"vless","settings":{"vnext":[{"address":"edge.example.com"}]}},
        {"protocol":"vmess","settings":{"vnext":[{"address":"backup.example.com"}]}}
    ]}))
    .unwrap();
    assert_eq!(
        runtime["dns"]["servers"][1]["domains"],
        serde_json::json!(["full:backup.example.com", "full:edge.example.com"])
    );
    for outbound in &runtime["outbounds"].as_array().unwrap()[..2] {
        assert_eq!(
            outbound["streamSettings"]["sockopt"]["domainStrategy"],
            "ForceIP"
        );
    }
}

#[test]
fn live_dns_rejects_localhost_implicit_matches_before_any_core_is_started() {
    for domain in [
        "edge",
        "edge.local",
        "edge.localdomain",
        "edge.localhost",
        "edge.lan",
        "edge.home.arpa",
        "edge.example",
        "edge.invalid",
        "edge.test",
        "EDGE.LOCAL.",
    ] {
        assert!(
            live_dns_overlay(serde_json::json!({
                "outbounds":[{"settings":{"address":domain}}]
            }))
            .is_err(),
            "localhost would outrank endpoint DNS for {domain}"
        );
    }
}

#[test]
fn runtime_xray_overlay_binds_every_outbound_without_mutating_snapshot() {
    let xray = br#"{
  "inbounds": [],
  "outbounds": [
    {
      "protocol": "shadowsocks",
      "settings": { "address": "edge.example.com" },
      "streamSettings": {
        "finalmask": { "enabled": true },
        "sockopt": { "tcpFastOpen": true, "interface": "OldTunnel" }
      }
    },
    {
      "protocol": "shadowsocks",
      "settings": { "servers": [{ "address": "backup.example.com" }] },
      "streamSettings": { "network": "tcp" }
    }
  ]
}"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: JsonValue =
        serde_json::from_slice(&fs::read(published.xray_config).unwrap()).unwrap();
    let outbounds = runtime["outbounds"].as_array().unwrap();

    for outbound in &outbounds[..2] {
        assert_eq!(
            outbound["streamSettings"]["sockopt"]["interface"],
            "Ethernet"
        );
        assert_eq!(
            outbound["streamSettings"]["sockopt"]["domainStrategy"],
            "ForceIP"
        );
    }
    assert_eq!(outbounds[0]["streamSettings"]["finalmask"]["enabled"], true);
    assert_eq!(
        outbounds[0]["streamSettings"]["sockopt"]["tcpFastOpen"],
        true
    );
    assert_eq!(outbounds[1]["streamSettings"]["network"], "tcp");
    assert!(runtime["dns"].get("hosts").is_none());
    assert_eq!(
        runtime["dns"]["servers"][1]["domains"],
        serde_json::json!(["full:backup.example.com", "full:edge.example.com"])
    );
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[test]
fn xray_server_domain_extraction_supports_finalmask_and_standard_shadowsocks_shapes() {
    let snapshot = Snapshot::parse(
        b"proxies: []\n",
        br#"{
          "outbounds": [
            {"settings": {"address": "edge.example.com"}},
            {"settings": {"servers": [
              {"address": "backup.example.com"},
              {"address": "203.0.113.10"},
              {"address": "edge.example.com"}
            ]}}
          ]
        }"#,
    )
    .unwrap();

    assert_eq!(
        xray_outbound_server_domains(&snapshot).unwrap(),
        vec![
            "backup.example.com".to_owned(),
            "edge.example.com".to_owned()
        ]
    );
}

#[test]
fn runtime_xray_overlay_treats_null_optional_sections_as_absent() {
    let xray = br#"{
      "dns": null,
      "outbounds": [
        {"settings": {"address": "edge.example.com"}, "streamSettings": null},
        {"streamSettings": {"sockopt": null}}
      ]
    }"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Wi-Fi")
        .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: JsonValue =
        serde_json::from_slice(&fs::read(published.xray_config).unwrap()).unwrap();

    assert!(runtime["dns"].get("hosts").is_none());
    assert_eq!(
        runtime["outbounds"][0]["streamSettings"]["sockopt"]["interface"],
        "Wi-Fi"
    );
    assert_eq!(
        runtime["outbounds"][1]["streamSettings"]["sockopt"]["interface"],
        "Wi-Fi"
    );
}

#[test]
fn runtime_xray_overlay_rejects_an_empty_outbound_set() {
    let xray = br#"{"outbounds": []}"#;
    let snapshot = Snapshot::parse(b"proxies: []\n", xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();

    assert!(publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).is_err());
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[cfg(windows)]
#[test]
#[ignore = "requires explicit local Xray binary and subscription config paths"]
fn pinned_xray_accepts_the_live_dns_runtime_overlay() {
    let binary = std::env::var_os("MULTICORE_TEST_XRAY_BIN").unwrap();
    let xray_path = std::env::var_os("MULTICORE_TEST_XRAY_CONFIG").unwrap();
    let xray = fs::read(xray_path).unwrap();
    let snapshot = Snapshot::parse(b"proxies: []\n", &xray).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface(multicore_core::default_ipv4_interface().unwrap())
        .unwrap();
    let directory = TempDirectory::new();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();

    use std::os::windows::process::CommandExt;
    let status = Command::new(binary)
        .creation_flags(0x0800_0000)
        .args(["run", "-test", "-c"])
        .arg(published.xray_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "multicore-runtime-control-test-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn runtime_publish_overrides_only_mihomo_controller_settings() {
    let mihomo = br#"
proxies: []
external-controller: 0.0.0.0:9090
external-controller-unix: /tmp/unsafe.sock
external-controller-pipe: unsafe-pipe
external-controller-tls: 0.0.0.0:9443
external-controller-cors: {allow-private-network: true}
external-controller-routing-mark: 1234
allow-lan: true
secret: old-secret
profile:
  store-selected: true
  store-fake-ip: true
external-ui: ./ui
external-ui-name: old-ui
external-ui-url: https://example.invalid/ui.zip
external-ui-custom-future-key: unsafe
external-doh-server: https://example.invalid/dns-query
global-client-fingerprint: chrome
cors:
  allow-origins: ['*']
"#;
    let xray = br#"{ "marker": "must remain byte exact" }"#;
    let snapshot = Snapshot::parse(mihomo, xray).unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new(
        "127.0.0.1:19090".parse::<SocketAddr>().unwrap(),
        "ephemeral-secret",
    )
    .unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let mapping = runtime.as_mapping().unwrap();
    let get = |key: &str| mapping.get(Value::String(key.to_owned()));

    assert_eq!(
        get("external-controller").and_then(Value::as_str),
        Some("127.0.0.1:19090")
    );
    assert_eq!(
        get("secret").and_then(Value::as_str),
        Some("ephemeral-secret")
    );
    for key in [
        "external-controller-unix",
        "external-controller-pipe",
        "external-controller-tls",
        "external-controller-cors",
        "external-controller-routing-mark",
        "cors",
        "external-doh-server",
        "global-client-fingerprint",
        "external-ui",
        "external-ui-name",
        "external-ui-url",
        "external-ui-custom-future-key",
    ] {
        assert!(get(key).is_none(), "unsafe runtime key remained: {key}");
    }
    assert_eq!(fs::read(published.xray_config).unwrap(), xray);
    let profile = get("profile").and_then(Value::as_mapping).unwrap();
    assert_eq!(
        profile
            .get(Value::String("store-selected".to_owned()))
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        profile
            .get(Value::String("store-fake-ip".to_owned()))
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(snapshot.mihomo.raw_yaml().as_bytes(), mihomo);
    assert_eq!(snapshot.xray.raw_bytes(), xray);
}

#[test]
fn runtime_publish_removes_superseded_secret_generations_only_after_success() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let first_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "old-secret").unwrap();
    let first =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &first_control).unwrap();
    assert!(first.directory.is_dir());
    let unrelated = directory.path().join("runtime-not-a-generation");
    fs::create_dir(&unrelated).unwrap();

    let second_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "new-secret").unwrap();
    let second =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &second_control).unwrap();
    assert!(second.directory.is_dir());
    assert!(!first.directory.exists());
    assert!(unrelated.exists());
    let published: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("runtime-")
                .is_some_and(|suffix| {
                    suffix.len() == 20 && suffix.bytes().all(|b| b.is_ascii_digit())
                })
        })
        .collect();
    assert_eq!(published.len(), 1);
}

#[test]
fn failed_runtime_publication_keeps_the_last_complete_generation() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let first = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let blocked = directory.path().join(format!(
        ".runtime-00000000000000000002.staging-{}",
        std::process::id()
    ));
    fs::create_dir(&blocked).unwrap();

    assert!(publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).is_err());
    assert!(first.directory.is_dir());
    assert!(first.mihomo_config.is_file());
    assert!(first.xray_config.is_file());
}

#[test]
fn staged_runtime_keeps_old_active_until_commit_and_abort_removes_only_new() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let old_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "old-secret").unwrap();
    let old =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &old_control).unwrap();

    let new_control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "new-secret").unwrap();
    let staged =
        stage_runtime_with_mihomo_control(&snapshot, directory.path(), &new_control).unwrap();
    let staged_paths = staged.paths().clone();
    assert!(old.directory.is_dir());
    assert!(staged_paths.directory.is_dir());
    staged.abort().unwrap();
    assert!(old.directory.is_dir());
    assert!(!staged_paths.directory.exists());

    let staged =
        stage_runtime_with_mihomo_control(&snapshot, directory.path(), &new_control).unwrap();
    let committed = staged.commit().unwrap();
    assert!(committed.directory.is_dir());
    assert!(!old.directory.exists());
}

#[cfg(windows)]
#[test]
fn cleanup_failure_does_not_cancel_committed_runtime_and_is_retried_later() {
    use std::os::windows::fs::OpenOptionsExt;

    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let old = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&old.mihomo_config)
        .unwrap();

    let staged = stage_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let active = staged.commit().unwrap();
    assert!(active.directory.is_dir());
    assert!(old.directory.is_dir());

    drop(locked);
    let staged = stage_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let newest = staged.commit().unwrap();
    assert!(newest.directory.is_dir());
    assert!(!old.directory.exists());
    assert!(!active.directory.exists());
}

#[test]
fn runtime_publish_injects_configured_external_ui_directory() {
    let directory = TempDirectory::new();
    let dashboard = directory.path().join("dashboard");
    fs::create_dir(&dashboard).unwrap();
    fs::write(dashboard.join("index.html"), b"<html></html>").unwrap();

    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret")
        .unwrap()
        .with_external_ui_directory(&dashboard)
        .unwrap();

    let published = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control)
        .expect("staging with a valid external-ui directory must succeed");
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    let mapping = runtime.as_mapping().unwrap();
    assert_eq!(
        mapping
            .get(Value::String("external-ui".to_owned()))
            .and_then(Value::as_str),
        Some(dashboard.to_str().unwrap())
    );
    // Never our own external-ui-name/url alongside it: Mihomo's own auto-update fields must
    // stay absent so it can never attempt a network fetch for the dashboard it already has.
    for key in ["external-ui-name", "external-ui-url"] {
        assert!(
            mapping.get(Value::String(key.to_owned())).is_none(),
            "unexpected {key} in runtime overlay"
        );
    }
}

#[test]
fn runtime_publish_omits_external_ui_when_not_configured() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();

    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let runtime: Value =
        serde_yaml_ng::from_slice(&fs::read(published.mihomo_config).unwrap()).unwrap();
    assert!(
        runtime
            .as_mapping()
            .unwrap()
            .get(Value::String("external-ui".to_owned()))
            .is_none()
    );
}

#[test]
fn external_ui_directory_rejects_relative_missing_and_indexless_paths() {
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();

    assert!(
        control
            .clone()
            .with_external_ui_directory("relative/dashboard")
            .is_err(),
        "relative path must be rejected"
    );
    assert!(
        control
            .clone()
            .with_external_ui_directory(directory.path().join("does-not-exist"))
            .is_err(),
        "missing directory must be rejected"
    );
    let empty_dashboard = directory.path().join("empty-dashboard");
    fs::create_dir(&empty_dashboard).unwrap();
    assert!(
        control
            .with_external_ui_directory(&empty_dashboard)
            .is_err(),
        "directory without index.html must be rejected"
    );
}

#[test]
fn runtime_control_rejects_non_loopback_controller() {
    assert!(MihomoRuntimeControl::new("192.0.2.1:19090".parse().unwrap(), "secret").is_err());
}

#[test]
fn runtime_control_debug_output_redacts_secret() {
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "must-never-be-logged")
            .unwrap();
    let debug = format!("{control:?}");
    assert!(debug.contains("[REDACTED]"));
    assert!(!debug.contains("must-never-be-logged"));
}

#[cfg(windows)]
#[test]
fn runtime_paths_have_a_protected_single_principal_dacl() {
    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();

    for path in [
        directory.path(),
        published.directory.as_path(),
        published.mihomo_config.as_path(),
        published.xray_config.as_path(),
    ] {
        assert_protected_single_principal_dacl(path);
    }
    assert!(!fs::read(&published.mihomo_config).unwrap().is_empty());
    assert_eq!(fs::read(&published.xray_config).unwrap(), br#"{}"#);
    let snapshot_root = directory.path().join("snapshots");
    let store = PersistentSnapshotStore::open(&snapshot_root).unwrap();
    store.commit(snapshot).unwrap();
    let generation = snapshot_root.join("snapshot-00000000000000000001");
    let mihomo = generation.join("mihomo.yaml");
    let xray = generation.join("xray.json");
    for path in [
        snapshot_root.as_path(),
        generation.as_path(),
        mihomo.as_path(),
        xray.as_path(),
    ] {
        assert_protected_single_principal_dacl(path);
    }
}

#[cfg(windows)]
fn assert_protected_single_principal_dacl(path: &Path) {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
            GetSecurityDescriptorControl, GetTokenInformation, OBJECT_INHERIT_ACE,
            PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(
        status,
        ERROR_SUCCESS,
        "failed to inspect {}",
        path.display()
    );
    assert!(!dacl.is_null());
    let mut control = 0_u16;
    let mut revision = 0_u32;
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
        0
    );
    assert_ne!(control & SE_DACL_PROTECTED, 0, "{}", path.display());
    let mut size = ACL_SIZE_INFORMATION::default();
    assert_ne!(
        unsafe {
            GetAclInformation(
                dacl,
                (&mut size as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        },
        0
    );
    assert_eq!(
        size.AceCount,
        1,
        "unexpected ACL entries for {}",
        path.display()
    );
    let mut raw_ace = ptr::null_mut();
    assert_ne!(unsafe { GetAce(dacl, 0, &mut raw_ace) }, 0);
    let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(ace.Header.AceType, 0, "ACE must be ACCESS_ALLOWED");
    assert_eq!(ace.Mask, FILE_ALL_ACCESS);
    let expected_inheritance = if fs::metadata(path).unwrap().is_dir() {
        (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8
    } else {
        0
    };
    assert_eq!(
        ace.Header.AceFlags & (CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE) as u8,
        expected_inheritance
    );

    let mut token = ptr::null_mut();
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
        0
    );
    let mut bytes_needed = 0_u32;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes_needed);
    }
    assert_ne!(bytes_needed, 0);
    let word_size = std::mem::size_of::<usize>();
    let mut token_info = vec![0_usize; (bytes_needed as usize).div_ceil(word_size)];
    assert_ne!(
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast(),
                bytes_needed,
                &mut bytes_needed,
            )
        },
        0
    );
    let token_user = unsafe { &*token_info.as_ptr().cast::<TOKEN_USER>() };
    let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>();
    assert_ne!(unsafe { EqualSid(ace_sid, token_user.User.Sid) }, 0);
    unsafe { CloseHandle(token) };
    unsafe { LocalFree(descriptor.cast()) };
}

#[cfg(unix)]
#[test]
fn runtime_paths_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDirectory::new();
    let snapshot = Snapshot::parse(b"proxies: []\n", br#"{}"#).unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "secret").unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    for path in [directory.path(), published.directory.as_path()] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [published.mihomo_config, published.xray_config] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let snapshot_root = directory.path().join("snapshots");
    let store = PersistentSnapshotStore::open(&snapshot_root).unwrap();
    store.commit(snapshot).unwrap();
    let generation = snapshot_root.join("snapshot-00000000000000000001");
    for path in [&snapshot_root, &generation] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [generation.join("mihomo.yaml"), generation.join("xray.json")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn mihomo_only_runtime_keeps_native_proxies_and_an_idle_xray() {
    let snapshot = Snapshot::parse(
        include_bytes!("fixtures/remnawave-multicore-mihomo.yaml"),
        multicore_core::IDLE_XRAY_CONFIG,
    )
    .unwrap();
    let directory = TempDirectory::new();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral")
        .unwrap()
        .with_xray_outbound_interface("Ethernet")
        .unwrap();
    let published =
        publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let xray: JsonValue =
        serde_json::from_slice(&fs::read(&published.xray_config).unwrap()).unwrap();
    assert!(
        xray.get("inbounds")
            .and_then(JsonValue::as_array)
            .is_none_or(Vec::is_empty)
    );
    let outbounds = xray["outbounds"].as_array().unwrap();
    assert_eq!(outbounds.len(), 1);
    assert_eq!(outbounds[0]["tag"], multicore_core::IDLE_XRAY_OUTBOUND_TAG);
    let mihomo: Value =
        serde_yaml_ng::from_slice(&fs::read(&published.mihomo_config).unwrap()).unwrap();
    assert_eq!(mihomo["proxies"][0]["type"], Value::from("vless"));
    assert_eq!(mihomo["tun"]["enable"], Value::from(true));
}

#[test]
fn runtime_overlay_pins_gvisor_unless_the_provider_chose_a_stack() {
    let control =
        MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "ephemeral").unwrap();
    for (yaml, expected) in [
        (
            "tun:
  enable: true
proxies: []
",
            "gvisor",
        ),
        (
            "tun:
  enable: true
  stack: system
proxies: []
",
            "system",
        ),
        (
            "proxies: []
",
            "gvisor",
        ),
    ] {
        let snapshot = Snapshot::parse(yaml.as_bytes(), b"{}").unwrap();
        let directory = TempDirectory::new();
        let published =
            publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
        let mihomo: Value =
            serde_yaml_ng::from_slice(&fs::read(&published.mihomo_config).unwrap()).unwrap();
        assert_eq!(mihomo["tun"]["stack"], Value::from(expected), "{yaml}");
    }
}
