//! Offscreen UI previews rendered with Slint's software renderer.
//!
//! Run with:
//! `cargo test -p multicore-desktop --features preview-render -- --ignored render_ui_previews`
//!
//! Screenshots are written to `artifacts/screenshots/<MULTICORE_PREVIEW_SET or "current">`.
//! No window, GPU, daemon, or user data directory is touched.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{PlatformError, WindowAdapter};
use slint::{ComponentHandle, ModelRc, PhysicalSize, Rgb8Pixel, SharedString, VecModel};

use crate::{
    AppWindow, TrayMenuRow, TrayPopup, UiCatalogGroup, UiCatalogNode, UiEvent, UiProxyMapping,
    UiSubscriptionProfile,
};

thread_local! {
    static LAST_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct PreviewPlatform;

impl slint::platform::Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        LAST_WINDOW.with(|last| *last.borrow_mut() = Some(window.clone()));
        Ok(window)
    }
}

fn output_dir() -> PathBuf {
    let set = std::env::var("MULTICORE_PREVIEW_SET").unwrap_or_else(|_| "current".into());
    let set = set
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>();
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts/screenshots")
        .join(if set.is_empty() {
            "current".into()
        } else {
            set
        })
}

fn settle() {
    // Let entry animations (menu fade, modal fade) finish before capture.
    for _ in 0..4 {
        std::thread::sleep(Duration::from_millis(120));
        slint::platform::update_timers_and_animations();
    }
}

fn capture(component: &impl ComponentHandle, name: &str, width: u32, height: u32) {
    let window = LAST_WINDOW
        .with(|last| last.borrow().clone())
        .expect("preview window adapter");
    window.set_size(PhysicalSize::new(width, height));
    component.show().expect("show preview component");
    settle();
    window.request_redraw();
    let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); (width * height) as usize];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width as usize);
    });
    component.hide().expect("hide preview component");
    let bytes = buffer
        .iter()
        .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
        .collect::<Vec<_>>();
    let image = image::RgbImage::from_raw(width, height, bytes).expect("preview buffer size");
    let directory = output_dir();
    std::fs::create_dir_all(&directory).expect("create preview directory");
    let path = directory.join(format!("{name}-{width}x{height}.png"));
    image.save(&path).expect("save preview png");
    println!("{}", path.display());
}

fn s(value: &str) -> SharedString {
    value.into()
}

fn groups() -> ModelRc<UiCatalogGroup> {
    let rows = [
        ("g1", "🌍", "Сервер", true),
        ("g2", "🎮", "Игры", false),
        ("g3", "📺", "Стриминг", false),
        ("g4", "", "Прочее", false),
    ]
    .into_iter()
    .map(|(id, icon, label, selected)| UiCatalogGroup {
        id: s(id),
        icon: s(icon),
        icon_image: crate::emoji_images::image_for(icon),
        label: s(label),
        selected,
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn nodes(pending_selected: bool) -> ModelRc<UiCatalogNode> {
    let rows = [
        ("n1", "🌍", "Автовыбор", true, "42 мс", "success"),
        ("n2", "🇦🇱", "Albania", false, "96 мс", "warning"),
        ("n3", "🇩🇪", "Germany", false, "181 мс", "danger"),
        ("n4", "🇸🇪", "Sweden", false, "Тайм-аут", "danger"),
        ("n5", "🇳🇱", "Netherlands", false, "—", "neutral"),
        ("n6", "", "Без VPN", false, "", "neutral"),
    ]
    .into_iter()
    .map(|(id, icon, label, selected, latency, tone)| UiCatalogNode {
        id: s(id),
        icon: s(icon),
        icon_image: crate::emoji_images::image_for(icon),
        label: s(label),
        selected: selected && !pending_selected || (pending_selected && id == "n2"),
        latency_text: s(latency),
        latency_tone: s(tone),
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn profiles(count: usize) -> ModelRc<UiSubscriptionProfile> {
    let rows = [
        (
            "p1",
            "Локальный профиль",
            "874.9 GiB",
            "Срок неизвестен",
            true,
            true,
        ),
        (
            "p2",
            "Рабочий",
            "12.4 GiB / 100 GiB",
            "Осталось 12 дней",
            false,
            true,
        ),
        ("p3", "Поездки", "Нет данных", "Срок истёк", false, false),
        (
            "p4",
            "Резерв с очень длинным названием провайдера подписки",
            "1.0 GiB",
            "Осталось 1 день",
            false,
            true,
        ),
        ("p5", "Пятая", "Нет данных", "Срок неизвестен", false, true),
    ]
    .into_iter()
    .take(count)
    .map(
        |(id, title, usage, expiry, active, available)| UiSubscriptionProfile {
            id: s(id),
            title: s(title),
            usage: s(usage),
            expiry: s(expiry),
            active,
            available,
            has_logo: false,
            logo: Default::default(),
            fallback: s(&title.chars().next().unwrap().to_string()),
        },
    )
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn ready(ui: &AppWindow) {
    ui.set_has_profile(true);
    ui.set_status_eyebrow(s("Готово"));
    ui.set_headline(s("Можно подключаться"));
    ui.set_supporting(s(""));
    ui.set_node_icon_image(crate::emoji_images::image_for("🌍"));
    ui.set_node_icon(s("🌍"));
    ui.set_node_name(s("Автовыбор"));
    ui.set_primary_label(s("Подключиться"));
    ui.set_primary_enabled(true);
    ui.set_status_tone(s("accent"));
    ui.set_subscription_title(s("Локальный профиль"));
    ui.set_subscription_usage(s("874.9 GiB"));
    ui.set_subscription_expiry(s("Срок неизвестен"));
    ui.set_subscription_updated(s("27.09.2026"));
    ui.set_subscription_refresh_available(true);
    ui.set_subscription_refresh_enabled(true);
    ui.set_subscription_management_enabled(true);
    ui.set_active_profile_id(s("p1"));
    ui.set_runtime_versions(env!("MULTICORE_RUNTIME_VERSIONS").into());
    ui.set_subscription_profiles(profiles(3));
    ui.set_catalog_groups(groups());
    ui.set_catalog_nodes(nodes(false));
    ui.set_catalog_selection_enabled(false);
    ui.set_system_name(s("Windows"));
    ui.set_reduced_motion(true);
}

fn connected(ui: &AppWindow) {
    ui.set_node_latency(s("42 мс"));
    ready(ui);
    ui.set_status_eyebrow(s("Подключено"));
    ui.set_headline(s("Соединение защищено"));
    ui.set_supporting(s("Текущий маршрут"));
    ui.set_primary_label(s("Отключиться"));
    ui.set_status_tone(s("success"));
    ui.set_toggle_title(s("Защита включена"));
    ui.set_toggle_on(true);
    ui.set_session_uptime(s("00:12:43"));
    ui.set_exit_now_name(s("Sweden"));
    ui.set_exit_now_icon(s("🇸🇪"));
    ui.set_exit_now_icon_image(crate::emoji_images::image_for("🇸🇪"));
    ui.set_traffic_down(s("2.4 МБ/с"));
    ui.set_traffic_up(s("320 КБ/с"));
    ui.set_subscription_management_enabled(false);
    ui.set_profile_switch_locked(true);
    ui.set_catalog_selection_enabled(true);
}

fn events() -> ModelRc<UiEvent> {
    let rows = [
        ("21:04:11", "info", "Локальные SOCKS-мосты готовы."),
        ("21:04:12", "info", "Контроллер и TUN MultiCore готовы."),
        (
            "21:05:40",
            "error",
            "Не удалось применить локальную настройку маршрута для группы «Игры». Сервис вернул ошибку проверки конфигурации, повторите попытку позже или обновите подписку.",
        ),
    ]
    .into_iter()
    .map(|(timestamp, level, message)| UiEvent {
        timestamp: s(timestamp),
        component: s("system"),
        level: s(level),
        message: s(message),
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

/// Opens route search through the real filtering path (`apply_route_search`).
fn open_route_search(ui: &AppWindow, query: &str, loading: bool) {
    use crate::view_model::{CatalogNodePresentation, CatalogPresentation, CatalogSearchGroup};
    let node = |id: &str, label: &str, selected: bool, latency: &str, tone: &str| {
        CatalogNodePresentation {
            id: id.into(),
            label: label.into(),
            selected,
            delay_ms: None,
            latency_text: latency.into(),
            latency_tone: tone.into(),
        }
    };
    let catalog = CatalogPresentation {
        loading,
        search_groups: if loading {
            Vec::new()
        } else {
            vec![
                CatalogSearchGroup {
                    id: "g1".into(),
                    label: "🌍 Серверы".into(),
                    nodes: vec![
                        node("n1", "🌍 Автовыбор", true, "42 мс", "success"),
                        node("n2", "🇩🇪 Germany", false, "96 мс", "warning"),
                        node("n3", "🇸🇪 Sweden", false, "42 мс", "success"),
                    ],
                },
                CatalogSearchGroup {
                    id: "g2".into(),
                    label: "🎮 Игры".into(),
                    nodes: vec![node("n4", "🇩🇪 Germany · Games", true, "38 мс", "success")],
                },
            ]
        },
        ..CatalogPresentation::default()
    };
    ui.set_route_search_open(true);
    ui.set_route_search_query(query.into());
    crate::apply_route_search(ui, &catalog, query);
}

fn mappings() -> ModelRc<UiProxyMapping> {
    let rows = [
        ("🇦🇱 Albania", "127.0.0.1:20801", "albania-reality"),
        ("🇩🇪 Germany", "127.0.0.1:20802", "germany-xhttp"),
    ]
    .into_iter()
    .map(|(proxy, address, xray)| {
        let (icon, name) = proxy.split_once(' ').expect("flag prefix");
        UiProxyMapping {
            icon_image: crate::emoji_images::image_for(icon),
            proxy_name: s(name),
            address: s(address),
            xray_label: s(xray),
        }
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

type Scenario = (&'static str, u32, u32, fn(&AppWindow));

fn scenarios() -> Vec<Scenario> {
    vec![
        ("home-empty", 840, 720, |ui| {
            ui.set_reduced_motion(true);
        }),
        ("home-empty-min", 700, 620, |ui| {
            ui.set_reduced_motion(true);
        }),
        ("home-first-run-error", 840, 720, |ui| {
            ui.set_reduced_motion(true);
            ui.set_first_run_manual_open(true);
            ui.set_subscription_draft(s("https://example.com/sub"));
            ui.set_import_error(s(
                "Не удалось добавить подписку. Проверьте URL и повторите попытку.",
            ));
        }),
        ("home-ready", 840, 720, ready),
        ("home-ready-min", 700, 620, ready),
        // The window grows by the expanded rail (176px) so the content keeps its width.
        ("home-nav-expanded", 1016, 720, |ui| {
            connected(ui);
            ui.set_navigation_collapsed(false);
        }),
        ("home-catalog-loading", 840, 720, |ui| {
            ready(ui);
            ui.set_catalog_groups(ModelRc::new(VecModel::from(Vec::<UiCatalogGroup>::new())));
            ui.set_catalog_nodes(ModelRc::new(VecModel::from(Vec::<UiCatalogNode>::new())));
            ui.set_catalog_loading(true);
        }),
        ("home-connected", 840, 720, connected),
        ("home-pending-selection", 840, 720, |ui| {
            connected(ui);
            ui.set_catalog_nodes(nodes(true));
            ui.set_catalog_selection_pending(true);
        }),
        ("home-latency-loading", 840, 720, |ui| {
            ready(ui);
            ui.set_latency_check_loading(true);
            ui.set_subscription_updated_stale(true);
        }),
        ("home-announcement", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_announcement_text(s(
                "Плановые работы сегодня в 23:00 по московскому времени. В это время возможны кратковременные отключения серверов в Германии и Нидерландах.",
            ));
        }),
        ("home-search", 840, 720, |ui| {
            connected(ui);
            open_route_search(ui, "ger", false);
        }),
        ("home-search-empty", 840, 720, |ui| {
            connected(ui);
            open_route_search(ui, "zzz", false);
        }),
        ("home-search-loading", 840, 720, |ui| {
            connected(ui);
            open_route_search(ui, "ger", true);
        }),
        ("home-search-idle", 840, 720, |ui| {
            connected(ui);
            open_route_search(ui, "", false);
        }),
        ("home-announcement-open", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_announcement_text(s(
                "Плановые работы сегодня в 23:00 по московскому времени. В это время возможны кратковременные отключения серверов в Германии и Нидерландах.",
            ));
            ui.set_announcement_open(true);
        }),
        ("home-catalog-error", 840, 720, |ui| {
            ready(ui);
            ui.set_catalog_error(s(
                "Не удалось загрузить маршруты. Каталог устарел: обновите подписку, чтобы получить актуальный список серверов.",
            ));
        }),
        ("home-error", 840, 720, |ui| {
            ready(ui);
            ui.set_status_eyebrow(s("Нет связи с сервисом"));
            ui.set_headline(s("Сервис не отвечает"));
            ui.set_supporting(s("Фоновый сервис не ответил вовремя"));
            ui.set_node_name(s("—"));
            ui.set_primary_label(s("Проверить снова"));
            ui.set_status_tone(s("danger"));
            ui.set_toggle_title(s("Нет связи с сервисом"));
            ui.set_toggle_detail_is_exit(false);
            ui.set_subscription_refresh_error(s("Не удалось обновить. Рабочий профиль сохранён."));
        }),
        ("home-degraded", 840, 720, |ui| {
            ready(ui);
            ui.set_status_eyebrow(s("Сервис недоступен"));
            ui.set_headline(s("Ограниченный режим"));
            ui.set_supporting(s("Фоновый сервис отвечает не полностью."));
            ui.set_primary_label(s("Остановить туннель"));
            ui.set_status_tone(s("warning"));
            ui.set_toggle_title(s("Сервис недоступен"));
            ui.set_toggle_detail_is_exit(false);
            ui.set_subscription_refresh_available(false);
        }),
        // The approved home-state mockups (.superdesign home-notice/-error/-warning/-connecting).
        ("home-state-notice", 840, 720, |ui| {
            connected(ui);
            ui.set_subscription_title(s("Fjord VPN"));
            ui.set_subscription_expiry(s("Бессрочно"));
            ui.set_subscription_announcement_text(s("Плановые работы сегодня в 23:00 по московскому времени. Возможны кратковременные отключения серверов в Германии и Нидерландах."));
            ui.set_announcement_open(true);
        }),
        ("home-state-error", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_title(s("Fjord VPN"));
            ui.set_status_eyebrow(s("Не удалось подключиться"));
            ui.set_supporting(s(
                "Сервер не ответил за 15 с — повторите или выберите другой маршрут",
            ));
            ui.set_status_tone(s("danger"));
            ui.set_toggle_title(s("Не удалось подключиться"));
            ui.set_toggle_detail_is_exit(false);
            ui.set_subscription_refresh_error(s("Не удалось обновить подписку"));
            ui.set_catalog_nodes(ModelRc::default());
            ui.set_catalog_error(s(
                "Не удалось загрузить маршруты. Каталог устарел — обновите подписку, чтобы получить актуальный список серверов.",
            ));
        }),
        ("home-state-warning", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_title(s("Fjord VPN"));
            ui.set_subscription_expiry(s("Бессрочно"));
            ui.set_status_tone(s("warning"));
            ui.set_toggle_title(s("Нет связи с сервисом"));
            ui.set_supporting(s("Служба MultiCore не отвечает — перезапускаем…"));
            ui.set_toggle_detail_is_exit(false);
            ui.set_primary_enabled(false);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Запущен Happ — он перехватывает туннель. Закройте его, чтобы MultiCore мог подключиться.",
            ));
        }),
        ("home-state-connecting", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_title(s("Fjord VPN"));
            ui.set_subscription_expiry(s("Бессрочно"));
            ui.set_toggle_title(s("Подключение…"));
            ui.set_toggle_on(true);
            ui.set_toggle_busy(true);
            ui.set_primary_enabled(false);
            ui.set_update_available(true);
            ui.set_update_status(s("MultiCore 0.3.4 · установится после отключения"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("home-local-profile", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_refresh_available(false);
            ui.set_subscription_updated_stale(true);
        }),
        ("home-menu-mutation-error", 840, 720, |ui| {
            ready(ui);
            ui.set_profile_mutation_error(s(
                "Не удалось активировать подписку. Повторите попытку.",
            ));
            ui.set_active_panel(s("subscription"));
        }),
        ("home-update-banner-menu", 840, 720, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MultiCore 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_active_panel(s("subscription"));
        }),
        ("home-update-banner", 840, 720, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MultiCore 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("home-update-banner-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MultiCore 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("home-update-banner-longstatus-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s(
                "Доступна версия 0.4.0 с исправлениями безопасности и стабильности",
            ));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("home-conflicting-banner-min", 700, 620, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MultiCore. Закройте его перед подключением.",
            ));
        }),
        ("home-conflicting-banner", 840, 720, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MultiCore. Закройте его перед подключением.",
            ));
        }),
        // Realistic worst case: two conflicting apps plus the appended
        // "couldn't fully close it, it's a Windows service" note.
        ("home-conflicting-banner-worstcase-min", 700, 620, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружены другие VPN-приложения (FlClash, Happ): они могут конфликтовать с MultiCore. Закройте их перед подключением. Закрыть удалось не всё: FlClash запускает свой движок как службу Windows, которую наша кнопка не трогает без повышения прав. Отключитесь в самом приложении перед закрытием или остановите его службу в «Службах Windows».",
            ));
        }),
        ("home-conflicting-banner-menu-min", 700, 620, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MultiCore. Закройте его перед подключением.",
            ));
            ui.set_active_panel(s("subscription"));
        }),
        ("home-both-banners-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MultiCore 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MultiCore. Закройте его перед подключением.",
            ));
        }),
        ("home-ready-wide", 1600, 900, ready),
        ("home-update-banner-wide", 1600, 900, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MultiCore 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("subscriptions-wide", 1600, 900, |ui| {
            ready(ui);
            ui.set_subscription_profiles(profiles(5));
            ui.set_local_page(s("subscriptions"));
        }),
        ("status-wide", 1600, 900, |ui| {
            connected(ui);
            ui.set_local_page(s("status"));
            ui.set_events(events());
        }),
        ("home-menu-connected", 840, 720, |ui| {
            connected(ui);
            ui.set_subscription_profiles(profiles(4));
            ui.set_active_panel(s("subscription"));
        }),
        ("home-menu-list-error", 700, 620, |ui| {
            ready(ui);
            ui.set_profile_menu_error(s("Не удалось загрузить подписки. Повторите попытку."));
            ui.set_active_panel(s("subscription"));
        }),
        ("home-menu-loading", 700, 620, |ui| {
            ready(ui);
            ui.set_profile_menu_loading(true);
            ui.set_active_panel(s("subscription"));
        }),
        ("subscriptions-many", 840, 720, |ui| {
            ready(ui);
            ui.set_subscription_profiles(profiles(5));
            ui.set_local_page(s("subscriptions"));
        }),
        ("subscriptions-min", 700, 620, |ui| {
            ready(ui);
            ui.set_local_page(s("subscriptions"));
        }),
        ("subscriptions-connected", 840, 720, |ui| {
            connected(ui);
            ui.set_local_page(s("subscriptions"));
        }),
        ("subscriptions-editor-error", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("subscriptions"));
            ui.set_replace_editor_open(true);
            ui.set_subscription_draft(s("https://example.com/sub"));
            ui.set_import_error(s(
                "Не удалось добавить подписку. Проверьте URL и повторите попытку.",
            ));
        }),
        ("subscriptions-deep-link-error", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("subscriptions"));
            ui.set_deep_link_import_host(s("provider.example"));
            ui.set_import_error(s(
                "Не удалось добавить подписку. Проверьте URL и повторите попытку.",
            ));
        }),
        ("subscriptions-edit", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("subscriptions"));
            ui.set_profile_edit_id(s("p2"));
        }),
        ("subscriptions-delete", 700, 620, |ui| {
            ready(ui);
            ui.set_local_page(s("subscriptions"));
            ui.set_profile_delete_confirm_id(s("p2"));
            ui.set_profile_pending_operation(s("none"));
        }),
        ("subscriptions-list-error", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("subscriptions"));
            ui.set_subscription_profiles(profiles(0));
            ui.set_profile_menu_error(s("Не удалось загрузить подписки. Повторите попытку."));
        }),
        ("status", 840, 720, |ui| {
            connected(ui);
            ui.set_local_page(s("status"));
            ui.set_diagnostics_xray_value(s("Работает"));
            ui.set_diagnostics_xray_tone(s("success"));
            ui.set_diagnostics_mihomo_value(s("Работает"));
            ui.set_diagnostics_mihomo_tone(s("success"));
            ui.set_diagnostics_tun_value(s("Активен"));
            ui.set_diagnostics_tun_tone(s("success"));
            ui.set_diagnostic_mappings(mappings());
            ui.set_events(events());
        }),
        ("status-min", 700, 620, |ui| {
            connected(ui);
            ui.set_local_page(s("status"));
            ui.set_events(events());
        }),
        ("settings", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_launch_at_sign_in_enabled(true);
            ui.set_launch_at_sign_in_error(s(
                "Не удалось изменить автозапуск. Проверьте права доступа к реестру и повторите попытку.",
            ));
            ui.set_preference_error(s(
                "Не удалось сохранить настройки. Изменения применятся только до перезапуска.",
            ));
        }),
        ("settings-min", 700, 620, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_tun_toggle_available(true);
        }),
        ("settings-proxy-mode", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_tun_toggle_available(true);
            ui.set_tun_enabled(false);
        }),
        ("settings-updates", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_settings_section(s("updates"));
            ui.set_update_status(s("MultiCore 0.4.0 доступна"));
            ui.set_update_detail(s("Загрузим подписанный пакет и перезапустим клиент. Активный туннель будет корректно остановлен."));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("modal-update", 840, 720, |ui| {
            ready(ui);
            ui.set_update_confirm_open(true);
        }),
        ("modal-deep-link", 700, 620, |ui| {
            ready(ui);
            ui.set_deep_link_confirm_host(s("provider.example"));
            ui.set_deep_link_confirm_open(true);
        }),
    ]
}

fn tray_rows() -> (ModelRc<TrayMenuRow>, ModelRc<TrayMenuRow>) {
    let group =
        |id: &str, icon: &str, label: &str, detail: &str, expanded: bool, count: i32| TrayMenuRow {
            id: s(id),
            group_id: s(""),
            icon: s(icon),
            icon_image: crate::emoji_images::image_for(icon),
            label: s(label),
            detail: s(detail),
            selected: false,
            enabled: true,
            expanded,
            node_count: count,
        };
    let node = |id: &str, group_id: &str, icon: &str, label: &str, selected: bool| TrayMenuRow {
        id: s(id),
        group_id: s(group_id),
        icon: s(icon),
        icon_image: crate::emoji_images::image_for(icon),
        label: s(label),
        detail: s(""),
        selected,
        enabled: true,
        expanded: false,
        node_count: 0,
    };
    (
        ModelRc::new(VecModel::from(vec![
            group("g1", "🌍", "Сервер", "Автовыбор", true, 3),
            group("g2", "🎮", "Игры", "Germany", false, 0),
        ])),
        ModelRc::new(VecModel::from(vec![
            node("n1", "g1", "🌍", "Автовыбор", true),
            node("n2", "g1", "🇦🇱", "Albania", false),
            node("n3", "g1", "🇩🇪", "Germany", false),
        ])),
    )
}

#[test]
#[ignore = "writes preview screenshots; run explicitly with --features preview-render"]
fn render_ui_previews() {
    slint::platform::set_platform(Box::new(PreviewPlatform)).expect("install preview platform");
    for (name, width, height, setup) in scenarios() {
        let ui = AppWindow::new().expect("create app window");
        setup(&ui);
        capture(&ui, name, width, height);
    }

    // One window through expand → collapse, like a user toggling the rail while
    // the window grows and shrinks with it: the list must reflow both ways.
    let ui = AppWindow::new().expect("create app window");
    connected(&ui);
    ui.set_navigation_collapsed(false);
    capture(&ui, "rail-sequence-1-expanded", 1016, 720);
    ui.set_navigation_collapsed(true);
    capture(&ui, "rail-sequence-2-collapsed", 840, 720);

    // Real-world exit line (long node + group + speeds) at the minimum window size
    // must elide inside the card instead of widening the layout past the window.
    let ui = AppWindow::new().expect("create app window");
    connected(&ui);
    ui.set_node_name(s("Auto Sweden"));
    ui.set_exit_now_name(s("Sweden [se]"));
    ui.set_exit_now_icon_image(crate::emoji_images::image_for("🇸🇪"));
    ui.set_node_latency(s("94 ms"));
    ui.set_traffic_down(s("1016 Б/с"));
    ui.set_traffic_up(s("0 Б/с"));
    ui.set_session_uptime(s("00:00:32"));
    capture(&ui, "home-long-exit-min", 700, 620);

    let (groups, nodes) = tray_rows();
    let popup = TrayPopup::new().expect("create tray popup");
    popup.set_status_text(s("Подключено · Автовыбор"));
    popup.set_connection_label(s("Отключиться"));
    popup.set_connected(true);
    popup.set_group_rows(groups.clone());
    popup.set_node_rows(nodes.clone());
    capture(&popup, "tray-connected", 304, 460);

    let popup = TrayPopup::new().expect("create tray popup");
    popup.set_status_text(s("Профиль не добавлен"));
    popup.set_connection_label(s("Подключиться"));
    popup.set_connection_enabled(false);
    popup.set_has_profile(false);
    capture(&popup, "tray-no-profile", 304, 460);
}
