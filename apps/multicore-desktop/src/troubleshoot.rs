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
