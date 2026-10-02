//! «Устранить неполадки» on the desktop: when it is offered, and the state of one run (cards,
//! fix offers the user ticks, fix results). The checks and fixes themselves run in the daemon.

use multicore_core::troubleshoot::{Finding, FixOffer, FixReport, StepReport, Tone};

use crate::view_model::StatePresentation;

/// The home banner that offers troubleshooting after a failed connect. Not for the failures
/// the user caused or that troubleshooting cannot touch: a dismissed administrator prompt or
/// MULTICORE running in another user's session.
pub(crate) fn offered_after_failure(presentation: &StatePresentation) -> bool {
    presentation.has_profile
        && presentation.is_error
        && presentation.eyebrow == "Не удалось подключиться"
        && !presentation.supporting.contains("отменено")
        && !presentation.supporting.contains("другого пользователя")
}

/// «Переустановить службу» re-registers the machine-wide service, so it is offered on Windows
/// only to an install that has (or had) one; a per-user install installs it from Settings.
pub(crate) fn service_reinstall_available(
    windows: bool,
    per_user_install: bool,
    service_mode: bool,
    service_installed: bool,
) -> bool {
    windows && !per_user_install && (service_mode || service_installed)
}

pub(crate) fn tone_name(tone: Tone) -> &'static str {
    match tone {
        Tone::Error => "error",
        Tone::Warning => "warning",
        Tone::Info => "info",
        Tone::Success => "success",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Summary {
    pub(crate) tone: Tone,
    pub(crate) title: &'static str,
    pub(crate) detail: &'static str,
}

/// The headline of a finished run: errors, then warnings, otherwise all clear (information
/// cards such as «DNS-кэш очищен» do not count as problems).
pub(crate) fn summary(findings: &[Finding]) -> Summary {
    if findings.iter().any(|finding| finding.tone == Tone::Error) {
        Summary {
            tone: Tone::Error,
            title: "Найдены проблемы",
            detail: "Они мешают VPN. Ниже — что именно нашли и что можно исправить.",
        }
    } else if findings.iter().any(|finding| finding.tone == Tone::Warning) {
        Summary {
            tone: Tone::Warning,
            title: "Есть предупреждения",
            detail: "Это может мешать подключению. Посмотрите подробности ниже.",
        }
    } else {
        Summary {
            tone: Tone::Success,
            title: "Проблем не обнаружено",
            detail: "Система готова к подключению.",
        }
    }
}

/// The right-aligned label of a finding row: only what blocks VPN gets one.
pub(crate) fn finding_badge(tone: Tone) -> &'static str {
    match tone {
        Tone::Error => "Критично",
        Tone::Warning | Tone::Info | Tone::Success => "",
    }
}

/// Russian noun form for `count`: «1 проблема», «2 проблемы», «5 проблем».
fn plural<'a>(count: usize, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    match (count % 10, count % 100) {
        (1, rem) if rem != 11 => one,
        (2..=4, rem) if !(12..=14).contains(&rem) => few,
        _ => many,
    }
}

/// The header pill of a finished check: errors, else warnings, else all clear.
pub(crate) fn status_pill(findings: &[Finding]) -> (String, Tone) {
    let count = |tone: Tone| {
        findings
            .iter()
            .filter(|finding| finding.tone == tone)
            .count()
    };
    let (errors, warnings) = (count(Tone::Error), count(Tone::Warning));
    if errors > 0 {
        (
            format!(
                "{errors} {}",
                plural(errors, "проблема", "проблемы", "проблем")
            ),
            Tone::Error,
        )
    } else if warnings > 0 {
        (
            format!(
                "{warnings} {}",
                plural(
                    warnings,
                    "предупреждение",
                    "предупреждения",
                    "предупреждений"
                )
            ),
            Tone::Warning,
        )
    } else {
        ("Всё в порядке".to_owned(), Tone::Success)
    }
}

/// «последняя проверка …» in the page subtitle.
pub(crate) fn checked_ago(elapsed: std::time::Duration) -> String {
    let minutes = elapsed.as_secs() / 60;
    match minutes {
        0 => "только что".to_owned(),
        1..=59 => format!("{minutes} мин назад"),
        60..=1439 => format!("{} ч назад", minutes / 60),
        _ => format!("{} дн. назад", minutes / 1440),
    }
}

fn severity(tone: Tone) -> u8 {
    match tone {
        Tone::Error => 0,
        Tone::Warning => 1,
        Tone::Info => 2,
        Tone::Success => 3,
    }
}

/// One troubleshooting run.
#[derive(Clone, Debug, Default)]
pub(crate) struct TroubleshootRun {
    findings: Vec<Finding>,
    /// Offers with whether the user ticked them. Nothing is ticked by default.
    offers: Vec<(FixOffer, bool)>,
    fix_results: Vec<Finding>,
    reboot_recommended: bool,
}

impl TroubleshootRun {
    pub(crate) fn add_step(&mut self, report: StepReport) {
        self.findings.extend(report.findings);
        for offer in report.fixes {
            if !self.offers.iter().any(|(known, _)| known.id == offer.id) {
                self.offers.push((offer, false));
            }
        }
        // Stable: within a tone the check order is kept.
        self.findings.sort_by_key(|finding| severity(finding.tone));
    }

    pub(crate) fn findings(&self) -> &[Finding] {
        &self.findings
    }

    pub(crate) fn offers(&self) -> &[(FixOffer, bool)] {
        &self.offers
    }

    pub(crate) fn fix_results(&self) -> &[Finding] {
        &self.fix_results
    }

    pub(crate) fn reboot_recommended(&self) -> bool {
        self.reboot_recommended
    }

    pub(crate) fn toggle(&mut self, index: usize) {
        if let Some((_, checked)) = self.offers.get_mut(index) {
            *checked = !*checked;
        }
    }

    pub(crate) fn checked_ids(&self) -> Vec<String> {
        self.offers
            .iter()
            .filter(|(_, checked)| *checked)
            .map(|(offer, _)| offer.id.clone())
            .collect()
    }

    pub(crate) fn checked_count(&self) -> usize {
        self.offers.iter().filter(|(_, checked)| *checked).count()
    }

    /// The answer to running `ids`: their offers are gone (done or failed, the result card
    /// says which), the results come first, newest on top.
    pub(crate) fn apply_report(&mut self, ids: &[String], report: FixReport) {
        self.offers.retain(|(offer, _)| !ids.contains(&offer.id));
        let mut results = report.findings;
        results.append(&mut self.fix_results);
        self.fix_results = results;
        self.reboot_recommended |= report.reboot_recommended;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view_model::UiState;
    use multicore_core::troubleshoot::FixReport;

    fn finding(tone: Tone, title: &str) -> Finding {
        Finding::new(tone, title, "")
    }

    fn offer(id: &str) -> FixOffer {
        FixOffer {
            id: id.into(),
            title: id.into(),
            detail: String::new(),
            needs_admin: true,
            needs_reboot: false,
            title_key: None,
            detail_key: None,
        }
    }

    #[test]
    fn a_failed_connect_offers_troubleshooting_unless_the_user_caused_it() {
        let failed = |message: &str| {
            UiState::Error {
                message: message.into(),
            }
            .presentation()
        };
        assert!(offered_after_failure(&failed(
            "Не удалось запустить Mihomo. Проверьте исполняемые файлы."
        )));
        assert!(offered_after_failure(&failed(
            "Xray не успел запуститься вовремя. Повторите попытку."
        )));
        assert!(!offered_after_failure(&failed(
            "Подключение отменено: не выдано разрешение администратора."
        )));
        assert!(!offered_after_failure(&failed(
            "MULTICORE уже запущен у другого пользователя."
        )));
        assert!(!offered_after_failure(
            &UiState::Ready {
                profile: "p".into(),
                node: "n".into()
            }
            .presentation()
        ));
        assert!(!offered_after_failure(
            &UiState::ServiceUnreachable {
                message: "x".into()
            }
            .presentation()
        ));
    }

    #[test]
    fn retrying_after_troubleshooting_connects_instead_of_refreshing() {
        use crate::daemon::{DaemonStatus, test_support::MockDaemonClient};
        use crate::view_model::{DesktopViewModel, PrimaryAction};
        use std::sync::Arc;
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = model.begin_refresh();
        model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "error".into(),
                message: Some("Не удалось запустить Mihomo. Проверьте исполняемые файлы.".into()),
                ..DaemonStatus::default()
            }),
        );
        assert!(offered_after_failure(&model.presentation()));
        assert_eq!(model.presentation().primary_action, PrimaryAction::Retry);
        model.prepare_retry_connect();
        assert!(!offered_after_failure(&model.presentation()));
        assert!(matches!(
            model.begin_primary_action(),
            Some((PrimaryAction::Connect, _))
        ));
    }

    #[test]
    fn the_service_reinstall_is_only_for_machine_wide_windows_installs() {
        assert!(service_reinstall_available(true, false, true, true));
        assert!(
            service_reinstall_available(true, false, false, true),
            "a registered but unreachable service is exactly what it is for"
        );
        assert!(!service_reinstall_available(true, true, false, false));
        assert!(!service_reinstall_available(true, false, false, false));
        assert!(!service_reinstall_available(false, false, true, true));
    }

    #[test]
    fn the_summary_reads_errors_then_warnings_else_all_clear() {
        assert_eq!(summary(&[]).title, "Проблем не обнаружено");
        assert_eq!(summary(&[]).detail, "Система готова к подключению.");
        assert_eq!(
            summary(&[finding(Tone::Info, "DNS-кэш очищен")]).tone,
            Tone::Success
        );
        assert_eq!(
            summary(&[finding(Tone::Info, "a"), finding(Tone::Warning, "b")]).tone,
            Tone::Warning
        );
        assert_eq!(
            summary(&[finding(Tone::Warning, "a"), finding(Tone::Error, "b")]).title,
            "Найдены проблемы"
        );
    }

    #[test]
    fn the_header_pill_counts_problems_then_warnings_in_russian() {
        let many = |tone: Tone, count: usize| {
            (0..count)
                .map(|index| finding(tone, &index.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            status_pill(&many(Tone::Error, 1)),
            ("1 проблема".to_owned(), Tone::Error)
        );
        assert_eq!(status_pill(&many(Tone::Error, 2)).0, "2 проблемы");
        assert_eq!(status_pill(&many(Tone::Error, 5)).0, "5 проблем");
        assert_eq!(status_pill(&many(Tone::Error, 11)).0, "11 проблем");
        assert_eq!(status_pill(&many(Tone::Error, 22)).0, "22 проблемы");
        let mut mixed = many(Tone::Warning, 3);
        mixed.extend(many(Tone::Info, 2));
        assert_eq!(
            status_pill(&mixed),
            ("3 предупреждения".to_owned(), Tone::Warning)
        );
        assert_eq!(
            status_pill(&many(Tone::Info, 2)),
            ("Всё в порядке".to_owned(), Tone::Success)
        );
        assert_eq!(finding_badge(Tone::Error), "Критично");
        assert_eq!(finding_badge(Tone::Warning), "");
    }

    #[test]
    fn the_page_lives_under_settings_with_a_fixed_header_and_a_sticky_action_bar() {
        let page = include_str!("../ui/troubleshoot.slint").replace("\r\n", "\n");
        let app = include_str!("../ui/app.slint").replace("\r\n", "\n");
        // No «‹ Назад»: a breadcrumb to Settings, a status pill, «Проверить заново» and ✕.
        assert!(!page.contains("‹ Назад"));
        assert!(page.contains("text: \"Настройки\";"));
        assert!(page.contains("crumb-touch := TouchArea { mouse-cursor: pointer; clicked => { root.open-settings(); } }"));
        assert!(page.contains("label: \"Проверить заново\";"));
        assert!(page.contains("accessible-label: \"Закрыть\";"));
        // Header, then the scrolling body, then the action bar.
        let header = page.find("// ---- Header").expect("header");
        let body = page.find("scroll := OverlayScroll").expect("body");
        let bar = page.find("// ---- Action bar").expect("action bar");
        assert!(header < body && body < bar);
        assert!(page.contains("\"Исправить выбранное (\" + root.checked-count + \")\""));
        assert!(page.contains("label: \"Повторить подключение\";"));
        // ✕ and «Повторить подключение» go back where the page was opened from; the
        // breadcrumb always goes to Settings, which stays lit in the rail.
        let usage = app
            .split("if root.visible-page == \"troubleshoot\": TroubleshootPage {")
            .nth(1)
            .and_then(|source| {
                source
                    .split("if root.visible-page == \"subscriptions\"")
                    .next()
            })
            .expect("troubleshoot page usage");
        let close = usage
            .split("close => {")
            .nth(1)
            .expect("close")
            .split('}')
            .next()
            .unwrap();
        assert!(close.contains("root.local-page = root.troubleshoot-return-page;"));
        let settings = usage
            .split("open-settings => {")
            .nth(1)
            .expect("crumb")
            .split('}')
            .next()
            .unwrap();
        assert!(settings.contains("root.local-page = \"settings\";"));
        let retry = usage
            .split("retry-connect => {")
            .nth(1)
            .expect("retry")
            .split('}')
            .next()
            .unwrap();
        assert!(retry.contains("root.local-page = root.troubleshoot-return-page;"));
        assert!(app.contains(
            "active: root.visible-page == \"settings\" || root.visible-page == \"troubleshoot\";"
        ));
    }

    #[test]
    fn the_last_check_time_reads_as_minutes_hours_or_days() {
        use std::time::Duration;
        assert_eq!(checked_ago(Duration::from_secs(59)), "только что");
        assert_eq!(checked_ago(Duration::from_secs(120)), "2 мин назад");
        assert_eq!(checked_ago(Duration::from_secs(3 * 3600)), "3 ч назад");
        assert_eq!(checked_ago(Duration::from_secs(2 * 86_400)), "2 дн. назад");
    }

    #[test]
    fn steps_accumulate_sorted_by_severity_with_unticked_unique_offers() {
        let mut run = TroubleshootRun::default();
        run.add_step(StepReport {
            findings: vec![finding(Tone::Info, "dns"), finding(Tone::Warning, "port")],
            fixes: vec![offer("winsock-reset")],
        });
        run.add_step(StepReport {
            findings: vec![
                finding(Tone::Error, "kaspersky"),
                finding(Tone::Warning, "vpn"),
            ],
            fixes: vec![offer("winsock-reset"), offer("resync-time")],
        });
        let titles = run
            .findings()
            .iter()
            .map(|finding| finding.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["kaspersky", "port", "vpn", "dns"]);
        assert_eq!(run.offers().len(), 2);
        assert!(run.offers().iter().all(|(_, checked)| !checked));
        assert!(run.checked_ids().is_empty());
        run.toggle(1);
        run.toggle(7);
        assert_eq!(run.checked_ids(), ["resync-time"]);
        assert_eq!(run.checked_count(), 1);
        run.toggle(1);
        assert_eq!(run.checked_count(), 0);
    }

    #[test]
    fn applied_fixes_leave_the_list_and_their_results_stack_newest_first() {
        let mut run = TroubleshootRun::default();
        run.add_step(StepReport {
            findings: Vec::new(),
            fixes: vec![offer("winsock-reset"), offer("resync-time")],
        });
        run.toggle(0);
        let ids = run.checked_ids();
        run.apply_report(
            &ids,
            FixReport {
                findings: vec![finding(Tone::Success, "winsock")],
                reboot_recommended: true,
            },
        );
        assert_eq!(run.offers().len(), 1);
        assert_eq!(run.offers()[0].0.id, "resync-time");
        run.apply_report(
            &["reinstall-wintun".into()],
            FixReport {
                findings: vec![finding(Tone::Success, "wintun")],
                reboot_recommended: false,
            },
        );
        let results = run
            .fix_results()
            .iter()
            .map(|finding| finding.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(results, ["wintun", "winsock"]);
        assert!(
            run.reboot_recommended(),
            "a reboot once needed stays needed"
        );
    }
}
