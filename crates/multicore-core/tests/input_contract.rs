use multicore_core::{ConfigError, parse_mihomo, parse_xray};

const MIHOMO: &str = r#"tun:
  enable: true
dns:
  enable: true
proxies:
  - name: NL-1
    type: socks5
    server: 127.0.0.1
    port: 31080
  - name: US-1
    type: socks5
    server: 127.0.0.1
    port: 31081
proxy-groups:
  - name: Proxy
    type: select
    proxies: [NL-1, US-1]
rules:
  - MATCH,Proxy
"#;

/// Group members the way Mihomo v1.19.32 builds them (adapter/outboundgroup/parser.go and
/// groupbase.go): explicit `proxies`, then with `include-all`/`include-all-proxies` every
/// config proxy (sorted by name) matching `filter`; `exclude-filter` and `exclude-type` then
/// drop members, explicit ones included. Checked against a running Mihomo.
const MIHOMO_GROUP_RULES: &str = r#"
proxies:
  - {name: b-node, type: vless}
  - {name: "a-node Torrent ", type: vless}
  - {name: C ss, type: ss}
  - {name: "🇨🇳 Китай *", type: vless}
proxy-groups:
  - name: Auto
    type: url-test
    exclude-filter: (?i)torrent
    proxies: [b-node, "a-node Torrent "]
  - name: Main
    type: select
    exclude-type: Shadowsocks
    proxies: [Auto, DIRECT, b-node, b-node, "🇨🇳 Китай *"]
  - name: Torrent
    type: select
    include-all-proxies: true
    filter: (?i)(torrent|торрент)
    proxies: [DIRECT]
  - name: All
    type: select
    include-all: true
    exclude-type: shadowsocks|Hysteria2
  - name: Internal
    type: select
    hidden: true
    proxies: [b-node]
  - name: Excluded
    type: select
    exclude-filter: "Torrent`^b-"
    proxies: [b-node, "a-node Torrent "]
  - name: Unsupported pattern
    type: select
    exclude-filter: (?<=x)b
    proxies: [b-node]
"#;

#[test]
fn group_members_follow_mihomo_include_and_exclude_rules() {
    let parsed = parse_mihomo(MIHOMO_GROUP_RULES.as_bytes()).unwrap();
    let members = |name: &str| {
        let group = parsed
            .groups()
            .iter()
            .find(|group| group.name == name)
            .unwrap();
        group
            .members
            .iter()
            .map(|member| (member.name.as_str(), member.offered))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        members("Auto"),
        [("b-node", true), ("a-node Torrent ", false)]
    );
    assert_eq!(
        members("Main"),
        [
            ("Auto", true),
            ("DIRECT", true),
            ("b-node", true),
            ("🇨🇳 Китай *", true)
        ]
    );
    assert_eq!(
        members("Torrent"),
        [("DIRECT", true), ("a-node Torrent ", true)]
    );
    // Byte order, like Go's slices.Sort; `ss` is Mihomo's "Shadowsocks".
    assert_eq!(
        members("All"),
        [
            ("C ss", false),
            ("a-node Torrent ", true),
            ("b-node", true),
            ("🇨🇳 Китай *", true)
        ]
    );
    assert_eq!(
        members("Excluded"),
        [("b-node", false), ("a-node Torrent ", false)]
    );
    // A pattern Mihomo's regexp2 accepts but Rust's regex does not excludes nothing.
    assert_eq!(members("Unsupported pattern"), [("b-node", true)]);
    let hidden: Vec<_> = parsed
        .groups()
        .iter()
        .filter(|group| group.hidden)
        .map(|group| group.name.as_str())
        .collect();
    assert_eq!(hidden, ["Internal"]);
}

#[test]
fn mihomo_is_utf8_yaml_mapping_and_extracts_ui_catalog() {
    let parsed = parse_mihomo(MIHOMO.as_bytes()).unwrap();
    assert_eq!(parsed.proxy_names(), &["NL-1", "US-1"]);
    assert_eq!(parsed.groups().len(), 1);
    assert_eq!(parsed.groups()[0].name, "Proxy");
    assert_eq!(parsed.groups()[0].group_type, "select");
    assert_eq!(parsed.groups()[0].proxies, ["NL-1", "US-1"]);

    for invalid in [
        br#"{"proxies":[]}"#.as_slice(),
        br#"["vmess://one"]"#.as_slice(),
        b"dm1lc3M6Ly9leGFtcGxl".as_slice(),
        b"vless://synthetic\nvmess://synthetic".as_slice(),
        &[0xff, 0xfe],
    ] {
        assert!(parse_mihomo(invalid).is_err());
    }

    let oversized = vec![b'a'; 32 * 1024 * 1024 + 1];
    assert_eq!(parse_mihomo(&oversized), Err(ConfigError::TooLarge));
}

#[test]
fn xray_is_opaque_json_and_preserves_exact_input() {
    let raw = b"{\n  \"providerOwned\": true,\n  \"opaque\": [3, 2, 1]\n}\n";
    let parsed = parse_xray(raw).unwrap();
    assert_eq!(parsed.raw_bytes(), raw);
    assert_eq!(parsed.raw_json(), std::str::from_utf8(raw).unwrap());

    assert!(parse_xray(br#"[{"config":1}]"#).is_err());
    assert!(parse_xray(br#"[]"#).is_err());
    assert!(parse_xray(br#""vless://synthetic""#).is_err());
    assert!(parse_xray(b"{broken").is_err());
    assert!(parse_xray(&[0xff]).is_err());
}

#[test]
fn mihomo_extracts_only_unique_literal_loopback_socks_bridges() {
    let parsed = parse_mihomo(
        br#"proxies:
  - { name: Safe One, type: socks5, server: 127.0.0.1, port: 31080 }
  - { name: Safe One, type: socks5, server: 127.0.0.1, port: 31080 }
  - { name: Safe One, type: socks5, server: 127.0.0.1, port: 31099 }
  - { name: Other Loopback, type: SOCKS5, server: 127.0.0.1, port: 65535 }
  - { name: Remote, type: socks5, server: 192.0.2.1, port: 31081 }
  - { name: Hostname, type: socks5, server: localhost, port: 31082 }
  - { name: Zero, type: socks5, server: 127.0.0.1, port: 0 }
  - { name: Too Large, type: socks5, server: 127.0.0.1, port: 65536 }
  - { name: Wrong Type, type: http, server: 127.0.0.1, port: 31083 }
"#,
    )
    .unwrap();

    let bridges = parsed.loopback_socks_mappings();
    assert_eq!(bridges.len(), 2);
    assert_eq!(bridges[0].name, "Safe One");
    assert_eq!(bridges[0].port, 31080);
    assert_eq!(bridges[1].name, "Other Loopback");
    assert_eq!(bridges[1].port, 65535);
}
