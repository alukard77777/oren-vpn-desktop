use std::sync::Mutex;

use multicore_core::elevation_protocol::BrokerErrorCode;
use multicore_core::troubleshoot::{
    DpiService, FixId, FixOutcome, ListenPort, PrivilegedFix, Tone,
};

use super::*;
use crate::service_client::ServiceError;

fn tones(findings: &[multicore_core::troubleshoot::Finding]) -> Vec<Tone> {
    findings.iter().map(|finding| finding.tone).collect()
}

#[test]
fn kaspersky_and_dr_web_are_errors_and_other_antivirus_warnings() {
    let found = classify_antivirus([
        "AVP21.3",
        "ekrn",
        "Spooler",
        "spider",
        "AVPX",
        "avast! Antivirus",
        "klnagent",
        "WinDefend",
    ]);
    assert_eq!(
        found,
        [
            Antivirus {
                name: "Dr.Web",
                severe: true
            },
            Antivirus {
                name: "Kaspersky",
                severe: true
            },
            Antivirus {
                name: "Avast",
                severe: false
            },
            Antivirus {
                name: "ESET",
                severe: false
            },
        ]
    );
    let findings = antivirus_findings(&found);
    assert_eq!(
        tones(&findings),
        [Tone::Error, Tone::Error, Tone::Warning, Tone::Warning]
    );
    assert_eq!(findings[1].title, "Обнаружен Kaspersky");
    assert!(findings[1].detail.contains("выключите"));
    assert!(classify_antivirus(["WinDefend", "Spooler", "AVPX"]).is_empty());
}

#[test]
fn every_dpi_service_is_explained_and_offered_for_removal_but_folders_only_warned() {
    let report = dpi_report(
        &[
            DpiServiceState {
                service: DpiService::Zapret,
                running: true,
            },
            DpiServiceState {
                service: DpiService::WinDivert,
                running: false,
            },
        ],
        &[r"C:\Users\me\Downloads\zapret".to_owned()],
    );
    assert_eq!(
        tones(&report.findings),
        [Tone::Error, Tone::Warning, Tone::Warning]
    );
    assert!(report.findings[2].detail.contains("не трогает"));
    let ids = report
        .fixes
        .iter()
        .map(|offer| offer.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        ["remove-dpi-service:zapret", "remove-dpi-service:windivert"]
    );
    assert!(report.fixes.iter().all(|offer| offer.needs_admin));
    assert!(dpi_report(&[], &[]).findings.is_empty());
}

#[test]
fn only_other_programs_on_the_subscription_ports_are_reported() {
    let wanted = [
        ListenPort {
            port: 10808,
            udp: false,
            mihomo: false,
        },
        ListenPort {
            port: 1053,
            udp: true,
            mihomo: true,
        },
    ];
    let root = r"C:\Program Files\MultiCore\current";
    let owner = |port, udp, name: &str, image: Option<&str>| PortOwner {
        port,
        udp,
        process_name: Some(name.to_owned()),
        image: image.map(str::to_owned),
    };
    let owners = [
        // Our own Xray (the service's warm cores).
        owner(
            10808,
            false,
            "xray.exe",
            Some(r"C:\Program Files\MultiCore\current\cores\xray.exe"),
        ),
        // A foreign Xray on the same port, twice (IPv4 and IPv6).
        owner(
            10808,
            false,
            "xray.exe",
            Some(r"C:\Users\me\v2rayN\bin\xray.exe"),
        ),
        owner(
            10808,
            false,
            "xray.exe",
            Some(r"C:\Users\me\v2rayN\bin\xray.exe"),
        ),
        // Our Mihomo running as SYSTEM: path unreadable, name is ours.
        owner(1053, true, "mihomo.exe", None),
        // Not a port we need.
        owner(7890, false, "clash.exe", None),
        PortOwner {
            port: 1053,
            udp: false,
            process_name: None,
            image: None,
        },
    ];
    let findings = port_findings(&wanted, &owners, Some(root));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].tone, Tone::Warning);
    assert_eq!(findings[0].title, "Порт 10808 занят");
    assert!(findings[0].detail.contains("«xray.exe»"));
    let unknown = port_findings(
        &wanted,
        &[PortOwner {
            port: 1053,
            udp: true,
            process_name: Some("dnscrypt-proxy.exe".into()),
            image: None,
        }],
        Some(root),
    );
    assert_eq!(unknown[0].title, "Порт 1053 (UDP) занят");
}

#[test]
fn our_loopback_proxy_is_recognised_and_every_proxy_outcome_is_explained() {
    let ours = crate::system_proxy::ProxySettings::applied(41_234);
    assert!(looks_like_our_proxy(
        ours.proxy_server.as_deref().unwrap(),
        ours.proxy_override.as_deref()
    ));
    assert!(!looks_like_our_proxy("127.0.0.1:41234", Some("<local>")));
    assert!(!looks_like_our_proxy(
        "proxy.corp:3128",
        Some(crate::system_proxy::PROXY_OVERRIDE)
    ));
    assert!(proxy_findings(&ProxyOutcome::Direct, None).is_empty());
    let foreign = proxy_findings(
        &ProxyOutcome::ForeignDisabled {
            server: "127.0.0.1:8080".into(),
        },
        Some(PacHost::Named("wpad.corp")),
    );
    assert_eq!(tones(&foreign), [Tone::Info, Tone::Warning]);
    assert!(foreign[0].detail.contains("127.0.0.1:8080"));
    assert!(foreign[1].detail.contains("wpad.corp"));
    assert_eq!(
        tones(&proxy_findings(&ProxyOutcome::StaleOursCleared, None)),
        [Tone::Info]
    );
    assert_eq!(
        tones(&proxy_findings(
            &ProxyOutcome::DisableFailed { server: "x".into() },
            None
        )),
        [Tone::Error]
    );
    let environment = proxy_environment_findings(&["https_proxy".into()]);
    assert_eq!(tones(&environment), [Tone::Warning]);
    assert!(environment[0].detail.starts_with("https_proxy."));
}

#[test]
fn drover_is_offered_for_deletion_only_while_discord_is_closed() {
    assert!(drover_report(None).findings.is_empty());
    let running = drover_report(Some(&DroverState {
        files: 2,
        discord_running: true,
    }));
    assert_eq!(tones(&running.findings), [Tone::Error]);
    assert!(running.findings[0].detail.contains("Закройте Discord"));
    assert!(running.fixes.is_empty());
    let closed = drover_report(Some(&DroverState {
        files: 2,
        discord_running: false,
    }));
    assert_eq!(tones(&closed.findings), [Tone::Warning]);
    assert_eq!(closed.fixes.len(), 1);
    assert_eq!(closed.fixes[0].id, "delete-drover");
    assert!(!closed.fixes[0].needs_admin);
}

#[test]
fn vpn_hosts_and_policies_cards() {
    assert_eq!(
        vpn_findings(&["Work VPN".into()])[0].title,
        "Активно VPN-подключение «Work VPN»"
    );
    assert!(vpn_findings(&[]).is_empty());
    let entries = ["a b", "c d", "e f", "g h"].map(str::to_owned);
    let hosts = hosts_findings(r"C:\Windows\System32\drivers\etc\hosts", &entries);
    assert_eq!(hosts[0].title, "В файле hosts есть записи: 4");
    assert!(hosts[0].detail.contains("«e f»"));
    assert!(!hosts[0].detail.contains("«g h»"));
    assert!(hosts[0].detail.contains("не меняет"));
    assert!(hosts_findings("/etc/hosts", &[]).is_empty());
    let policy = policy_findings(&["NC_LanChangeProperties"]);
    assert_eq!(tones(&policy), [Tone::Error]);
    assert!(policy[0].detail.contains("корпоративный или школьный"));
    assert!(policy_findings(&[]).is_empty());
}

#[test]
fn the_clock_is_reported_past_two_minutes_and_resync_is_offered_on_windows() {
    assert_eq!(clock_state(Some(119)), ClockState::InSync);
    assert_eq!(clock_state(Some(-120)), ClockState::InSync);
    assert_eq!(clock_state(Some(-121)), ClockState::Skewed(-121));
    assert_eq!(clock_state(None), ClockState::Unknown);
    assert!(clock_report(ClockState::InSync, true).findings.is_empty());
    let behind = clock_report(ClockState::Skewed(-600), true);
    assert_eq!(behind.findings[0].title, "Часы отстают на 10 мин");
    assert_eq!(behind.fixes[0].id, "resync-time");
    let ahead = clock_report(ClockState::Skewed(3 * 86_400), false);
    assert_eq!(ahead.findings[0].title, "Часы спешат на 72 ч");
    assert!(ahead.fixes.is_empty(), "no Windows resync on Linux");
    let unknown = clock_report(ClockState::Unknown, true);
    assert_eq!(tones(&unknown.findings), [Tone::Info]);
    assert_eq!(unknown.fixes.len(), 1);
}

#[test]
fn winsock_reset_is_always_offered_never_applied_and_warns_about_the_reboot() {
    let report = network_report(Some(true), None);
    assert_eq!(tones(&report.findings), [Tone::Info]);
    assert_eq!(report.fixes.len(), 1);
    let winsock = &report.fixes[0];
    assert_eq!(winsock.id, "winsock-reset");
    assert!(winsock.needs_reboot && winsock.needs_admin);
    assert!(winsock.detail.contains("перезагрузка"));
    let with_proxy = network_report(Some(false), Some("proxy.corp:3128"));
    assert_eq!(tones(&with_proxy.findings), [Tone::Warning, Tone::Warning]);
    assert_eq!(
        with_proxy
            .fixes
            .iter()
            .map(|offer| offer.id.as_str())
            .collect::<Vec<_>>(),
        ["winhttp-reset-proxy", "winsock-reset"]
    );
}

#[test]
fn fix_requests_accept_only_known_ids_and_keep_the_service_reinstall_alone() {
    assert_eq!(
        parse_fix_request(&[
            "winsock-reset".into(),
            "delete-drover".into(),
            "winsock-reset".into()
        ]),
        Some(vec![
            FixId::DeleteDrover,
            FixId::Privileged(PrivilegedFix::WinsockReset)
        ])
    );
    for bad in [
        vec![],
        vec!["remove-dpi-service:Spooler".to_owned()],
        vec!["cmd /c whoami".to_owned()],
        vec!["reinstall-service".to_owned(), "winsock-reset".to_owned()],
        vec!["delete-drover".to_owned(); MAX_FIX_REQUEST + 1],
    ] {
        assert_eq!(parse_fix_request(&bad), None, "{bad:?}");
    }
    assert_eq!(
        parse_fix_request(&["reinstall-service".into()]),
        Some(vec![FixId::Privileged(PrivilegedFix::ReinstallService)])
    );
}

struct FakeService {
    available: bool,
    answer: Result<Vec<FixOutcome>, ServiceError>,
    calls: Mutex<Vec<Vec<PrivilegedFix>>>,
}

impl FakeService {
    fn new(available: bool, answer: Result<Vec<FixOutcome>, ServiceError>) -> Self {
        Self {
            available,
            answer,
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl ServiceFixes for FakeService {
    fn service_available(&self) -> bool {
        self.available
    }
    fn run_fixes(&self, fixes: &[PrivilegedFix]) -> Result<Vec<FixOutcome>, ServiceError> {
        self.calls.lock().unwrap().push(fixes.to_vec());
        self.answer.clone()
    }
}

struct FakeHelper {
    answer: Result<FixOutcome, HelperError>,
    calls: Mutex<Vec<Vec<PrivilegedFix>>>,
}

impl FakeHelper {
    fn new(answer: Result<FixOutcome, HelperError>) -> Self {
        Self {
            answer,
            calls: Mutex::new(Vec::new()),
        }
    }
}

impl ElevatedFixes for FakeHelper {
    fn run_fixes(&self, fixes: &[PrivilegedFix]) -> Result<Vec<FixOutcome>, HelperError> {
        self.calls.lock().unwrap().push(fixes.to_vec());
        self.answer.map(|outcome| vec![outcome; fixes.len()])
    }
}

const WINSOCK: [PrivilegedFix; 1] = [PrivilegedFix::WinsockReset];

#[test]
fn the_service_runs_fixes_without_a_prompt_when_it_can() {
    let service = FakeService::new(true, Ok(vec![FixOutcome::Done]));
    let helper = FakeHelper::new(Ok(FixOutcome::Done));
    assert_eq!(
        run_privileged(&WINSOCK, Some(&service), &helper),
        Ok(vec![FixOutcome::Done])
    );
    assert_eq!(service.calls.lock().unwrap().len(), 1);
    assert!(helper.calls.lock().unwrap().is_empty());
}

#[test]
fn standard_users_unreachable_services_and_per_user_installs_use_the_uac_helper() {
    for service in [
        FakeService::new(
            true,
            Err(ServiceError::Rejected(
                BrokerErrorCode::AuthenticationFailed,
            )),
        ),
        FakeService::new(true, Err(ServiceError::Unavailable)),
        FakeService::new(false, Ok(vec![])),
    ] {
        let helper = FakeHelper::new(Ok(FixOutcome::NothingToDo));
        assert_eq!(
            run_privileged(&WINSOCK, Some(&service), &helper),
            Ok(vec![FixOutcome::NothingToDo])
        );
        assert_eq!(*helper.calls.lock().unwrap(), [WINSOCK.to_vec()]);
    }
    let helper = FakeHelper::new(Ok(FixOutcome::Done));
    assert_eq!(
        run_privileged(&WINSOCK, None, &helper),
        Ok(vec![FixOutcome::Done])
    );
    // The service never re-registers itself, even when it is there.
    let service = FakeService::new(true, Ok(vec![FixOutcome::Done]));
    let helper = FakeHelper::new(Ok(FixOutcome::Done));
    let reinstall = [PrivilegedFix::ReinstallService];
    assert_eq!(
        run_privileged(&reinstall, Some(&service), &helper),
        Ok(vec![FixOutcome::Done])
    );
    assert!(service.calls.lock().unwrap().is_empty());
    assert_eq!(helper.calls.lock().unwrap().len(), 1);
}

#[test]
fn an_old_or_busy_service_and_a_dismissed_prompt_are_explained_not_retried() {
    for (error, expected) in [
        (
            ServiceError::Rejected(BrokerErrorCode::InvalidRequest),
            BatchError::ServiceTooOld,
        ),
        (ServiceError::Busy, BatchError::Busy),
        (
            ServiceError::Rejected(BrokerErrorCode::NotReady),
            BatchError::TunnelUp,
        ),
        (ServiceError::TimedOut, BatchError::Failed),
    ] {
        let service = FakeService::new(true, Err(error));
        let helper = FakeHelper::new(Ok(FixOutcome::Done));
        assert_eq!(
            run_privileged(&WINSOCK, Some(&service), &helper),
            Err(expected)
        );
        assert!(helper.calls.lock().unwrap().is_empty(), "{error:?}");
    }
    let helper = FakeHelper::new(Err(HelperError::Cancelled));
    assert_eq!(
        run_privileged(&WINSOCK, None, &helper),
        Err(BatchError::Cancelled)
    );
    assert_eq!(
        batch_error_finding(BatchError::ServiceTooOld).title,
        "Служба MULTICORE устарела"
    );
}

#[test]
fn a_fix_run_reports_drover_and_every_privileged_outcome_and_the_reboot() {
    let service = FakeService::new(true, Ok(vec![FixOutcome::Done, FixOutcome::Failed]));
    let helper = FakeHelper::new(Ok(FixOutcome::Done));
    let fixes = [
        FixId::DeleteDrover,
        FixId::Privileged(PrivilegedFix::WinsockReset),
        FixId::Privileged(PrivilegedFix::RemoveDpiService {
            service: DpiService::GoodbyeDpi,
        }),
    ];
    let report = run_fixes(&fixes, &|| DroverRemoval::Removed, Some(&service), &helper);
    assert_eq!(
        tones(&report.findings),
        [Tone::Success, Tone::Success, Tone::Error]
    );
    assert_eq!(report.findings[1].title, "Настройки Winsock сброшены");
    assert_eq!(
        report.findings[2].title,
        "Не удалось удалить службу GoodbyeDPI"
    );
    assert!(report.reboot_recommended);
    // Drover alone needs no privileged run at all.
    let service = FakeService::new(true, Ok(vec![]));
    let report = run_fixes(
        &[FixId::DeleteDrover],
        &|| DroverRemoval::DiscordRunning,
        Some(&service),
        &helper,
    );
    assert_eq!(tones(&report.findings), [Tone::Error]);
    assert!(service.calls.lock().unwrap().is_empty());
    assert!(!report.reboot_recommended);
}

#[test]
fn wintun_and_service_reinstall_outcomes_have_their_own_cards() {
    assert_eq!(
        privileged_finding(PrivilegedFix::ReinstallWintun, FixOutcome::Done).title,
        "Адаптер Wintun удалён"
    );
    assert_eq!(
        privileged_finding(PrivilegedFix::ReinstallWintun, FixOutcome::NothingToDo).tone,
        Tone::Info
    );
    assert_eq!(
        privileged_finding(PrivilegedFix::ReinstallService, FixOutcome::Failed).tone,
        Tone::Error
    );
    assert!(!needs_reboot(
        PrivilegedFix::ReinstallWintun,
        FixOutcome::Done
    ));
    assert!(!needs_reboot(
        PrivilegedFix::WinsockReset,
        FixOutcome::Failed
    ));
}

/// Every card and offer the checks and fixes can produce, with representative arguments.
fn every_report() -> (Vec<Finding>, Vec<FixOffer>) {
    let mut findings = Vec::new();
    let mut fixes = Vec::new();
    findings.extend(antivirus_findings(&[
        Antivirus {
            name: "Kaspersky",
            severe: true,
        },
        Antivirus {
            name: "ESET",
            severe: false,
        },
    ]));
    for running in [true, false] {
        let report = dpi_report(
            &[
                DpiServiceState {
                    service: DpiService::WinDivert,
                    running,
                },
                DpiServiceState {
                    service: DpiService::Zapret,
                    running,
                },
            ],
            &["C:\\zapret".into()],
        );
        findings.extend(report.findings);
        fixes.extend(report.fixes);
    }
    let wanted = [
        ListenPort {
            port: 7890,
            udp: false,
            mihomo: true,
        },
        ListenPort {
            port: 7891,
            udp: true,
            mihomo: true,
        },
    ];
    findings.extend(port_findings(
        &wanted,
        &[
            PortOwner {
                port: 7890,
                udp: false,
                process_name: Some("clash.exe".into()),
                image: None,
            },
            PortOwner {
                port: 7891,
                udp: true,
                process_name: None,
                image: None,
            },
        ],
        None,
    ));
    for outcome in [
        ProxyOutcome::StaleOursCleared,
        ProxyOutcome::ForeignDisabled {
            server: "127.0.0.1:8080".into(),
        },
        ProxyOutcome::DisableFailed {
            server: "127.0.0.1:8080".into(),
        },
    ] {
        findings.extend(proxy_findings(&outcome, Some(PacHost::Named("wpad.corp"))));
    }
    findings.extend(proxy_findings(
        &ProxyOutcome::Direct,
        Some(PacHost::Unrecognized),
    ));
    findings.extend(proxy_environment_findings(&["https_proxy".into()]));
    findings.extend(vpn_findings(&["Work".into()]));
    for discord_running in [true, false] {
        let report = drover_report(Some(&DroverState {
            files: 2,
            discord_running,
        }));
        findings.extend(report.findings);
        fixes.extend(report.fixes);
    }
    findings.extend(hosts_findings("C:\\hosts", &["1.2.3.4 a.example".into()]));
    findings.extend(policy_findings(&NETWORK_POLICY_VALUES));
    for state in [
        ClockState::Skewed(600),
        ClockState::Skewed(-600),
        ClockState::Skewed(4 * 3600),
        ClockState::Skewed(-4 * 3600),
        ClockState::Unknown,
    ] {
        let report = clock_report(state, true);
        findings.extend(report.findings);
        fixes.extend(report.fixes);
    }
    for dns in [Some(true), Some(false)] {
        let report = network_report(dns, Some("proxy.corp:3128"));
        findings.extend(report.findings);
        fixes.extend(report.fixes);
    }
    for error in [
        BatchError::Cancelled,
        BatchError::ServiceTooOld,
        BatchError::Busy,
        BatchError::TunnelUp,
        BatchError::Failed,
    ] {
        findings.push(batch_error_finding(error));
    }
    for fix in [
        PrivilegedFix::WinsockReset,
        PrivilegedFix::WinhttpResetProxy,
        PrivilegedFix::ResyncTime,
        PrivilegedFix::RemoveDpiService {
            service: DpiService::GoodbyeDpi,
        },
        PrivilegedFix::ReinstallWintun,
        PrivilegedFix::ReinstallService,
    ] {
        for outcome in [
            FixOutcome::Done,
            FixOutcome::NothingToDo,
            FixOutcome::Failed,
        ] {
            findings.push(privileged_finding(fix, outcome));
        }
    }
    for removal in [
        DroverRemoval::Removed,
        DroverRemoval::NothingFound,
        DroverRemoval::DiscordRunning,
        DroverRemoval::Failed,
    ] {
        findings.push(drover_removal_finding(removal));
    }
    (findings, fixes)
}

#[test]
fn every_card_carries_codes_and_reads_in_english() {
    let (findings, fixes) = every_report();
    let texts = findings
        .iter()
        .map(|finding| {
            (
                finding.title_key.is_some() && finding.detail_key.is_some(),
                finding.localized_title(),
                finding.localized_detail(),
                finding.title.clone(),
            )
        })
        .chain(fixes.iter().map(|offer| {
            (
                offer.title_key.is_some() && offer.detail_key.is_some(),
                offer.localized_title(),
                offer.localized_detail(),
                offer.title.clone(),
            )
        }))
        .collect::<Vec<_>>();
    for (keyed, title, detail, russian) in &texts {
        assert!(keyed, "card without codes: {russian}");
        // Russian is the default language: the localized text is the text sent before.
        assert_eq!(title, russian);
        assert!(!detail.is_empty());
    }
    let _english = multicore_i18n::ThreadLanguage::set(multicore_i18n::Lang::En);
    let cyrillic = |text: &str| text.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c));
    for finding in &findings {
        for text in [finding.localized_title(), finding.localized_detail()] {
            assert!(!cyrillic(&text), "untranslated card text: {text}");
        }
    }
    for offer in &fixes {
        for text in [offer.localized_title(), offer.localized_detail()] {
            assert!(!cyrillic(&text), "untranslated offer text: {text}");
        }
    }
}

#[test]
fn the_unparsed_script_address_keeps_its_russian_text() {
    let findings = proxy_findings(&ProxyOutcome::Direct, Some(PacHost::Unrecognized));
    assert!(
        findings[0]
            .detail
            .starts_with("Адрес сценария: (адрес не распознан). ")
    );
    let clock = clock_report(ClockState::Skewed(-4 * 3600), false);
    assert_eq!(clock.findings[0].title, "Часы отстают на 4 ч");
    let clock = clock_report(ClockState::Skewed(150), false);
    assert_eq!(clock.findings[0].title, "Часы спешат на 3 мин");
    let ports = port_findings(
        &[ListenPort {
            port: 53,
            udp: true,
            mihomo: true,
        }],
        &[PortOwner {
            port: 53,
            udp: true,
            process_name: None,
            image: None,
        }],
        None,
    );
    assert_eq!(ports[0].title, "Порт 53 (UDP) занят");
    assert!(
        ports[0]
            .detail
            .starts_with("Его занимает другая программа, а он нужен подписке.")
    );
}
