//! The messages the daemon reports to the desktop, by stable code.
//!
//! The daemon sends a [`MessageKey`](crate::MessageKey) (code and arguments) next to the Russian
//! text it always sent; the desktop shows [`MessageKey::localize`](crate::MessageKey::localize)
//! and falls back to the Russian text only for a code it does not know. The Russian template is
//! the catalog `msgid`, so it is written once, here, and translated in the `.po` like every
//! other string. Codes never change meaning once released; a changed message gets a new code.
//!
//! Arguments are never translated: they are core names, ports, provider-chosen names, counts.

/// `(code, Russian template)`. Arguments fill `{0}`, `{1}`, … (see [`crate::format`]).
pub const ALL: &[(&str, &str)] = &[
    // Connection status line and the matching log events.
    ("connect.starting", "Запускается подключение."),
    (
        "connect.starting_confirm_admin",
        "Запускается подключение. Подтвердите права администратора в системном окне.",
    ),
    ("connect.tunnel_on", "Включается туннель…"),
    ("connect.established", "Подключение установлено."),
    ("connect.failed", "Запуск подключения завершился ошибкой."),
    ("connect.aborted", "Запуск подключения был прерван."),
    (
        "connect.interrupted",
        "Подключение прервано. Выполните отключение для очистки.",
    ),
    (
        "connect.rollback_incomplete",
        "Не удалось полностью отменить запуск. Выполните отключение для очистки.",
    ),
    (
        "connect.processes_unknown",
        "Состояние процессов неизвестно. Выполните отключение для очистки.",
    ),
    (
        "connect.prepare_failed",
        "Не удалось подготовить сетевой интерфейс и адреса узлов.",
    ),
    (
        "connect.prepare_failed_event",
        "Подготовка конфигурации перед подключением завершилась ошибкой.",
    ),
    (
        "connect.restaged",
        "Прогретая конфигурация не подошла, запуск подготовлен заново.",
    ),
    (
        "connect.activate_failed",
        "Не удалось активировать конфигурацию запуска.",
    ),
    (
        "connect.activate_failed_event",
        "Активация конфигурации перед подключением завершилась ошибкой.",
    ),
    (
        "connect.prepare_deferred",
        "Подготовка к подключению отложена до подключения.",
    ),
    ("disconnect.done", "Подключение остановлено."),
    (
        "disconnect.incomplete",
        "Не удалось полностью остановить подключение. Повторите попытку.",
    ),
    (
        "disconnect.failed_event",
        "Остановка подключения завершилась ошибкой.",
    ),
    (
        "cores.warm_up_failed",
        "Ядра не удалось запустить заранее. Они запустятся при подключении.",
    ),
    (
        "cores.lost",
        "Служба MULTICORE потеряла ядра — подключение разорвано. Подключитесь снова.",
    ),
    (
        "cores.lost_reconnecting",
        "Служба MULTICORE потеряла ядра — переподключаемся…",
    ),
    (
        "cores.reconnected",
        "Подключение восстановлено автоматически.",
    ),
    (
        "cores.reconnect_failed",
        "Не удалось переподключиться автоматически — подключитесь снова.",
    ),
    (
        "service.unavailable_notice",
        "Служба MULTICORE недоступна — права администратора будут запрошены при подключении.",
    ),
    ("subscription.switched", "Подписка переключена."),
    (
        "subscription.save_failed",
        "Не удалось сохранить изменение подписки.",
    ),
    ("route.selected", "Маршрут Mihomo изменён."),
    ("route.select_failed", "Mihomo не применил выбор маршрута."),
    (
        "troubleshoot.fixes_applied",
        "Устранение неполадок: {0}; ошибок: {1}.",
    ),
    // Why a connection did not start ({0} is Xray or Mihomo).
    (
        "start.elevation_cancelled",
        "Подключение отменено: не выдано разрешение администратора.",
    ),
    (
        "start.elevation_failed",
        "Не удалось запустить службу подключения с правами администратора.",
    ),
    (
        "start.broker_failed",
        "Служба подключения не ответила. Повторите попытку.",
    ),
    (
        "start.core_failed",
        "Не удалось запустить {0}. Подробности — в логе на странице «Состояние».",
    ),
    (
        "start.core_missing",
        "Не удалось запустить {0}: файл ядра не найден или заблокирован. Проверьте карантин антивируса и переустановите MULTICORE.",
    ),
    (
        "start.core_exited",
        "{0} завершился сразу после запуска. Причина — в логе на странице «Состояние».",
    ),
    (
        "start.core_timed_out",
        "{0} не успел запуститься вовремя. Повторите попытку.",
    ),
    (
        "start.tun_failed",
        "Не удалось включить TUN-адаптер. Повторите попытку; если не поможет — перезагрузите компьютер или подключитесь без TUN.",
    ),
    (
        "start.tun_timed_out",
        "TUN-адаптер не включился вовремя. Повторите попытку; если не поможет — перезагрузите компьютер или подключитесь без TUN.",
    ),
    (
        "start.system_proxy_failed",
        "Не удалось включить системный прокси. Повторите попытку.",
    ),
    (
        "start.service_busy",
        "MULTICORE уже запущен у другого пользователя.",
    ),
    (
        "start.unknown",
        "Не удалось запустить подключение. Повторите попытку.",
    ),
    // Troubleshooting: antivirus ({0} is the product).
    ("ts.antivirus.found", "Обнаружен {0}"),
    (
        "ts.antivirus.severe",
        "{0} часто блокирует VPN. Если подключение не работает — выключите его или добавьте MULTICORE в исключения.",
    ),
    (
        "ts.antivirus.mild",
        "Антивирус может мешать VPN. Если подключение не работает — добавьте MULTICORE в исключения или временно отключите защиту.",
    ),
    // Packet filters ({0} is the service).
    ("ts.dpi.running", "Работает {0}"),
    ("ts.dpi.installed", "Установлена служба {0}"),
    (
        "ts.dpi.driver_running",
        "Драйвер перехвата пакетов (его используют zapret и GoodbyeDPI) перехватывает трафик и мешает VPN.",
    ),
    (
        "ts.dpi.program_running",
        "Программа обхода блокировок перехватывает трафик и мешает VPN.",
    ),
    (
        "ts.dpi.driver_stopped",
        "Драйвер перехвата пакетов (его используют zapret и GoodbyeDPI): служба остановлена, но может запуститься снова.",
    ),
    (
        "ts.dpi.program_stopped",
        "Программа обхода блокировок: служба остановлена, но может запуститься снова.",
    ),
    ("ts.dpi.remove", "Остановить и удалить службу {0}"),
    (
        "ts.dpi.remove_detail",
        "Служба будет остановлена и удалена из Windows. Файлы программы останутся — при необходимости её можно установить снова.",
    ),
    ("ts.dpi.folders", "Найдены программы обхода блокировок"),
    (
        "ts.dpi.folders_detail",
        "{0}. Если они запущены — закройте их. MULTICORE эти папки не трогает.",
    ),
    // Ports ({0} port, {1} the program).
    ("ts.port.busy", "Порт {0} занят"),
    ("ts.port.busy_udp", "Порт {0} (UDP) занят"),
    (
        "ts.port.named_owner",
        "Его занимает «{0}», а он нужен подписке. Закройте эту программу и повторите подключение.",
    ),
    (
        "ts.port.unknown_owner",
        "Его занимает другая программа, а он нужен подписке. Закройте эту программу и повторите подключение.",
    ),
    // System proxy ({0} the proxy address or the script host).
    (
        "ts.proxy.stale_cleared",
        "Убран прокси от прошлого подключения",
    ),
    (
        "ts.proxy.stale_cleared_detail",
        "MULTICORE не успел выключить системный прокси. Прежние настройки восстановлены.",
    ),
    ("ts.proxy.disabled", "Системный прокси отключён"),
    (
        "ts.proxy.disabled_detail",
        "Был включён прокси {0} — он мешает подключению. Включить снова: Параметры → Сеть и Интернет → Прокси.",
    ),
    (
        "ts.proxy.disable_failed",
        "Не удалось отключить системный прокси",
    ),
    (
        "ts.proxy.disable_failed_detail",
        "Включён прокси {0}. Отключите его вручную: Параметры → Сеть и Интернет → Прокси.",
    ),
    ("ts.proxy.pac", "Включён сценарий настройки прокси"),
    (
        "ts.proxy.pac_detail",
        "Адрес сценария: {0}. Если подключение не работает, выключите «Использовать сценарий настройки» в Параметры → Сеть и Интернет → Прокси.",
    ),
    (
        "ts.proxy.pac_detail_unparsed",
        "Адрес сценария: (адрес не распознан). Если подключение не работает, выключите «Использовать сценарий настройки» в Параметры → Сеть и Интернет → Прокси.",
    ),
    ("ts.proxy.env", "Прокси задан в переменных окружения"),
    (
        "ts.proxy.env_detail",
        "{0}. Программы могут отправлять трафик мимо VPN — уберите эти переменные, если подключение не работает.",
    ),
    // Other VPNs ({0} the connection name).
    ("ts.vpn.active", "Активно VPN-подключение «{0}»"),
    (
        "ts.vpn.active_detail",
        "Два VPN одновременно конфликтуют. Отключите его: Параметры → Сеть и Интернет → VPN.",
    ),
    // Drover.
    ("ts.drover.found", "Найден Drover для Discord"),
    (
        "ts.drover.found_running",
        "Подменная version.dll в папке Discord пускает его трафик мимо VPN. Закройте Discord и повторите проверку — тогда её можно будет удалить.",
    ),
    (
        "ts.drover.found_detail",
        "Подменная version.dll в папке Discord пускает его трафик мимо VPN и может мешать подключению.",
    ),
    ("ts.drover.remove", "Удалить Drover из папки Discord"),
    (
        "ts.drover.remove_detail",
        "Будут удалены version.dll и drover.ini (файлов: {0}). Сам Discord продолжит работать.",
    ),
    ("ts.drover.removed", "Drover удалён"),
    (
        "ts.drover.removed_detail",
        "Файлы удалены из папок Discord.",
    ),
    ("ts.drover.not_found", "Drover не найден"),
    ("ts.nothing_to_remove", "Удалять нечего."),
    ("ts.drover.close_discord", "Закройте Discord и повторите"),
    (
        "ts.drover.discord_running",
        "Пока Discord запущен, его файлы удалить нельзя.",
    ),
    ("ts.drover.remove_failed", "Не удалось удалить Drover"),
    (
        "ts.drover.remove_failed_detail",
        "Закройте Discord и повторите.",
    ),
    // hosts ({0} count; {1} path, {2} examples).
    ("ts.hosts.entries", "В файле hosts есть записи: {0}"),
    (
        "ts.hosts.entries_detail",
        "{0}: {1}. Они могут перенаправлять сайты. MULTICORE этот файл не меняет — проверьте его вручную.",
    ),
    // Group policies ({0} the values).
    ("ts.policy.restricted", "Ограничения сети"),
    (
        "ts.policy.restricted_detail",
        "Групповые политики запрещают менять сетевые настройки ({0}). Возможно, это корпоративный или школьный ПК — обратитесь к его администратору.",
    ),
    // Clock ({0} minutes or hours).
    ("ts.clock.ahead_minutes", "Часы спешат на {0} мин"),
    ("ts.clock.ahead_hours", "Часы спешат на {0} ч"),
    ("ts.clock.behind_minutes", "Часы отстают на {0} мин"),
    ("ts.clock.behind_hours", "Часы отстают на {0} ч"),
    (
        "ts.clock.skewed_detail",
        "Неверное время ломает защищённые подключения. Включите «Установить время автоматически» или синхронизируйте часы.",
    ),
    ("ts.clock.unknown", "Не удалось сверить время"),
    (
        "ts.clock.unknown_detail",
        "Сервер проверки времени не ответил — возможно, нет доступа в интернет.",
    ),
    ("ts.clock.resync", "Синхронизировать время"),
    (
        "ts.clock.resync_detail",
        "Запустит службу времени Windows и сверит часы с сервером времени.",
    ),
    // Network.
    ("ts.network.dns_flushed", "DNS-кэш очищен"),
    (
        "ts.network.dns_flushed_detail",
        "Устаревшие записи DNS больше не помешают подключению.",
    ),
    ("ts.network.dns_flush_failed", "Не удалось очистить DNS-кэш"),
    (
        "ts.network.dns_flush_failed_detail",
        "Это не критично: кэш обновится сам или после перезагрузки.",
    ),
    ("ts.network.winhttp", "Задан прокси WinHTTP"),
    (
        "ts.network.winhttp_detail",
        "Системные службы Windows ходят через {0}. Это может мешать VPN.",
    ),
    ("ts.network.winhttp_reset", "Сбросить прокси WinHTTP"),
    (
        "ts.network.winhttp_reset_detail",
        "То же, что «netsh winhttp reset proxy»: службы Windows снова будут подключаться напрямую.",
    ),
    (
        "ts.network.winsock_reset",
        "Сбросить сетевые настройки (Winsock)",
    ),
    (
        "ts.network.winsock_reset_detail",
        "Восстанавливает сетевой стек Windows, если его испортила другая программа. Нужна перезагрузка ПК; некоторые VPN и антивирусы после сброса придётся переустановить.",
    ),
    // A privileged batch that did not run.
    ("ts.batch.cancelled", "Исправления отменены"),
    (
        "ts.batch.cancelled_detail",
        "Без разрешения администратора их не выполнить.",
    ),
    ("ts.batch.service_too_old", "Служба MULTICORE устарела"),
    (
        "ts.batch.service_too_old_detail",
        "Обновите MULTICORE — тогда служба сможет выполнять эти исправления.",
    ),
    ("ts.batch.busy", "MULTICORE запущен у другого пользователя"),
    (
        "ts.batch.busy_detail",
        "Исправления можно выполнить, когда он отключится.",
    ),
    ("ts.batch.tunnel_up", "Сначала отключитесь"),
    (
        "ts.batch.tunnel_up_detail",
        "Пока работает подключение, адаптер Wintun удалять нельзя.",
    ),
    ("ts.batch.failed", "Не удалось выполнить исправления"),
    (
        "ts.batch.failed_detail",
        "Запуск с правами администратора не удался. Повторите попытку.",
    ),
    // One finished privileged fix ({0} the service).
    ("ts.fix.dpi_removed", "Служба {0} удалена"),
    (
        "ts.fix.dpi_removed_detail",
        "Если она не успела остановиться, то исчезнет после перезагрузки.",
    ),
    ("ts.fix.dpi_absent", "Службы {0} уже нет"),
    ("ts.fix.dpi_remove_failed", "Не удалось удалить службу {0}"),
    (
        "ts.fix.dpi_remove_failed_detail",
        "Удалите программу через «Установка и удаление программ».",
    ),
    ("ts.fix.winhttp_done", "Прокси WinHTTP сброшен"),
    (
        "ts.fix.winhttp_done_detail",
        "Службы Windows подключаются напрямую.",
    ),
    (
        "ts.fix.winhttp_failed",
        "Не удалось сбросить прокси WinHTTP",
    ),
    (
        "ts.fix.winhttp_failed_detail",
        "Выполните «netsh winhttp reset proxy» от имени администратора.",
    ),
    ("ts.fix.time_done", "Время синхронизировано"),
    (
        "ts.fix.time_done_detail",
        "Часы сверены с сервером времени.",
    ),
    ("ts.fix.time_failed", "Не удалось синхронизировать время"),
    (
        "ts.fix.time_failed_detail",
        "Включите «Установить время автоматически» в Параметры → Время и язык.",
    ),
    ("ts.fix.winsock_done", "Настройки Winsock сброшены"),
    (
        "ts.fix.winsock_done_detail",
        "Перезагрузите ПК, чтобы изменения вступили в силу.",
    ),
    ("ts.fix.winsock_failed", "Не удалось сбросить Winsock"),
    (
        "ts.fix.winsock_failed_detail",
        "Выполните «netsh winsock reset» от имени администратора.",
    ),
    ("ts.fix.wintun_done", "Адаптер Wintun удалён"),
    (
        "ts.fix.wintun_done_detail",
        "MULTICORE создаст его заново при следующем подключении.",
    ),
    ("ts.fix.wintun_absent", "Адаптера Wintun нет"),
    (
        "ts.fix.wintun_absent_detail",
        "Он будет создан заново при следующем подключении.",
    ),
    ("ts.fix.wintun_failed", "Не удалось удалить адаптер Wintun"),
    (
        "ts.fix.wintun_failed_detail",
        "Перезагрузите ПК и повторите.",
    ),
    ("ts.fix.service_done", "Служба MULTICORE переустановлена"),
    (
        "ts.fix.service_done_detail",
        "Служба зарегистрирована заново и запущена.",
    ),
    ("ts.fix.service_failed", "Не удалось переустановить службу"),
    (
        "ts.fix.service_failed_detail",
        "Перезагрузите ПК и повторите или переустановите MULTICORE установщиком.",
    ),
    ("ts.linux.fixes_unavailable", "Доступно только в Windows"),
    (
        "ts.linux.fixes_unavailable_detail",
        "В Linux исправления выполняются вручную.",
    ),
];

/// The Russian template of a code.
#[must_use]
pub fn template(code: &str) -> Option<&'static str> {
    ALL.iter()
        .find(|(candidate, _)| *candidate == code)
        .map(|(_, template)| *template)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique() {
        let mut codes = ALL.iter().map(|(code, _)| *code).collect::<Vec<_>>();
        codes.sort_unstable();
        let total = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), total, "duplicate daemon message code");
    }
}
