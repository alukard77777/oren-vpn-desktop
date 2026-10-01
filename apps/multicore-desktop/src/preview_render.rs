//! Offscreen UI previews rendered with Slint's software renderer.
//!
//! Run with:
//! `cargo test -p multicore-desktop --features preview-render -- --ignored render_ui_previews`
//!
//! Screenshots are written to `artifacts/screenshots/<MULTICORE_PREVIEW_SET or "current">`.
//! `MULTICORE_PREVIEW_SCALE=1.25` renders at a display scale factor (pixel-alignment checks).
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

fn settle(steps: u32) {
    // Let entry animations (menu fade, modal fade) finish before capture.
    for _ in 0..steps {
        std::thread::sleep(Duration::from_millis(120));
        slint::platform::update_timers_and_animations();
    }
}

/// `MULTICORE_PREVIEW_ONLY=chip,tray` renders only scenarios starting with those prefixes.
fn wanted(name: &str) -> bool {
    std::env::var("MULTICORE_PREVIEW_ONLY").map_or(true, |only| {
        only.split(',')
            .map(str::trim)
            .any(|prefix| !prefix.is_empty() && name.starts_with(prefix))
    })
}

fn capture(component: &impl ComponentHandle, name: &str, width: u32, height: u32) {
    capture_after(component, name, width, height, 4);
}

fn capture_after(
    component: &impl ComponentHandle,
    name: &str,
    width: u32,
    height: u32,
    settle_steps: u32,
) {
    if !wanted(name) {
        return;
    }
    let window = LAST_WINDOW
        .with(|last| last.borrow().clone())
        .expect("preview window adapter");
    let scale: f32 = std::env::var("MULTICORE_PREVIEW_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0);
    let (width, height) = (
        (width as f32 * scale) as u32,
        (height as f32 * scale) as u32,
    );
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    window.set_size(PhysicalSize::new(width, height));
    component.show().expect("show preview component");
    settle(settle_steps);
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
        measuring: false,
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn profiles(count: usize) -> ModelRc<UiSubscriptionProfile> {
    // (id, title, usage, expiry, active, available, remaining, days, used, tone, expired)
    let rows = [
        (
            "p1",
            "Локальный профиль",
            "874.9 GiB",
            "Срок неизвестен",
            true,
            true,
            "∞ GiB",
            "Бессрочно",
            -1.0_f32,
            "accent",
            false,
        ),
        (
            "p2",
            "Рабочий",
            "12.4 GiB / 100 GiB",
            "Осталось 12 дней",
            false,
            true,
            "87.6 GiB",
            "12 дн.",
            0.124,
            "accent",
            false,
        ),
        (
            "p3",
            "Поездки",
            "Нет данных",
            "Срок истёк",
            false,
            false,
            "40.0 GiB",
            "Срок истёк",
            0.6,
            "danger",
            true,
        ),
        (
            "p4",
            "Резерв с очень длинным названием провайдера подписки",
            "1.0 GiB",
            "Осталось 1 день",
            false,
            true,
            "9.0 GiB",
            "1 дн.",
            0.9,
            "warning",
            false,
        ),
        (
            "p5",
            "Пятая",
            "Нет данных",
            "Срок неизвестен",
            false,
            true,
            "∞ GiB",
            "Бессрочно",
            -1.0,
            "accent",
            false,
        ),
    ]
    .into_iter()
    .take(count)
    .map(
        |(id, title, usage, expiry, active, available, remaining, days, used, tone, expired)| {
            UiSubscriptionProfile {
                id: s(id),
                title: s(title),
                usage: s(usage),
                expiry: s(expiry),
                active,
                available,
                has_logo: false,
                logo: Default::default(),
                logo_small: Default::default(),
                fallback: s(&title.chars().next().unwrap().to_string()),
                remaining: s(remaining),
                days: s(days),
                used_fraction: used.max(0.0),
                unlimited: used < 0.0,
                quota_tone: s(tone),
                expired,
            }
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

/// Title chip on Fjord VPN with a quota (remaining, days, share used, tone).
fn chip(ui: &AppWindow, remaining: &str, days: &str, used: f32, tone: &str) {
    connected(ui);
    ui.set_subscription_title(s("Fjord VPN"));
    ui.set_subscription_initial(s("F"));
    ui.set_subscription_remaining(s(remaining));
    ui.set_subscription_days(s(days));
    ui.set_subscription_used_fraction(used.max(0.0));
    ui.set_subscription_unlimited(used < 0.0);
    ui.set_subscription_quota_tone(s(tone));
}

fn chip_normal(ui: &AppWindow) {
    chip(ui, "87.6 GiB", "12 дн.", 0.124, "accent");
}

fn events() -> ModelRc<UiEvent> {
    let rows = [
        ("21:04:11", "info", "Локальные SOCKS-мосты готовы."),
        ("21:04:12", "info", "Контроллер и TUN MULTICORE готовы."),
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
            icon_id: None,
            measuring: false,
        }
    };
    let catalog = CatalogPresentation {
        loading,
        exit_latency_text: if loading {
            String::new()
        } else {
            "42 мс".into()
        },
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
        ("chip-normal", 840, 720, chip_normal),
        ("chip-warning", 840, 720, |ui| {
            chip(ui, "9.0 GiB", "2 дн.", 0.91, "warning");
        }),
        ("chip-expired", 840, 720, |ui| {
            chip(ui, "40.0 GiB", "Срок истёк", 0.6, "danger");
        }),
        ("chip-unlimited", 840, 720, |ui| {
            chip(ui, "∞ GiB", "Бессрочно", -1.0, "accent");
        }),
        ("chip-unlimited-days", 840, 720, |ui| {
            chip(ui, "∞ GiB", "12 дн.", -1.0, "accent");
        }),
        ("chip-refresh-error", 840, 720, |ui| {
            chip_normal(ui);
            ui.set_subscription_refresh_error(s("Не удалось обновить подписку"));
        }),
        ("chip-long-name-min", 700, 620, |ui| {
            chip_normal(ui);
            ui.set_subscription_title(s("Резерв с очень длинным названием провайдера подписки"));
        }),
        ("chip-long-name-expanded-min", 876, 620, |ui| {
            chip_normal(ui);
            ui.set_navigation_collapsed(false);
            ui.set_subscription_title(s("Резерв с очень длинным названием провайдера подписки"));
        }),
        ("chip-motion-1-refreshing", 840, 720, |ui| {
            chip_normal(ui);
            ui.set_subscription_refresh_pending(true);
            ui.set_chip_phase(s("refreshing"));
        }),
        ("chip-motion-3-hold", 840, 720, |ui| {
            chip(ui, "86.4 GiB", "12 дн.", 0.136, "accent");
            ui.set_chip_phase(s("hold"));
        }),
        ("chip-motion-4-settled", 840, 720, |ui| {
            chip(ui, "86.4 GiB", "12 дн.", 0.136, "accent");
        }),
        ("chip-menu", 840, 720, |ui| {
            chip_normal(ui);
            ui.set_profile_switch_locked(false);
            ui.set_subscription_profiles(profiles(5));
            ui.set_active_panel(s("subscription"));
        }),
        ("chip-menu-locked-min", 700, 620, |ui| {
            chip_normal(ui);
            ui.set_subscription_profiles(profiles(4));
            ui.set_active_panel(s("subscription"));
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
            ui.set_latency_check_available(true);
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
            ui.set_supporting(s("Служба MULTICORE не отвечает — перезапускаем…"));
            ui.set_toggle_detail_is_exit(false);
            ui.set_primary_enabled(false);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Запущен Happ — он перехватывает туннель. Закройте его, чтобы MULTICORE мог подключиться.",
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
            ui.set_update_status(s("MULTICORE 0.3.4 · установится после отключения"));
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
            ui.set_update_status(s("MULTICORE 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_active_panel(s("subscription"));
        }),
        ("home-update-banner", 840, 720, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
        }),
        ("home-update-banner-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 0.4.0"));
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
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MULTICORE. Закройте его перед подключением.",
            ));
        }),
        ("home-conflicting-banner", 840, 720, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MULTICORE. Закройте его перед подключением.",
            ));
        }),
        // Realistic worst case: two conflicting apps plus the appended
        // "couldn't fully close it, it's a Windows service" note.
        ("home-conflicting-banner-worstcase-min", 700, 620, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружены другие VPN-приложения (FlClash, Happ): они могут конфликтовать с MULTICORE. Закройте их перед подключением. Закрыть удалось не всё: FlClash запускает свой движок как службу Windows, которую наша кнопка не трогает без повышения прав. Отключитесь в самом приложении перед закрытием или остановите его службу в «Службах Windows».",
            ));
        }),
        ("home-conflicting-banner-menu-min", 700, 620, |ui| {
            ready(ui);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MULTICORE. Закройте его перед подключением.",
            ));
            ui.set_active_panel(s("subscription"));
        }),
        ("home-both-banners-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 0.4.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_conflicting_apps_banner_visible(true);
            ui.set_conflicting_apps_message(s(
                "Обнаружено другое VPN-приложение (FlClash): оно может конфликтовать с MULTICORE. Закройте его перед подключением.",
            ));
        }),
        ("home-handoff-toast", 840, 720, |ui| {
            ready(ui);
            ui.set_handoff_notice_for(s("/home/me/Apps/MultiCore-1.1.1-x86_64.AppImage"));
        }),
        ("home-handoff-and-update-toasts", 840, 720, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 1.2.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_handoff_notice_for(s("/home/me/Apps/MultiCore-1.1.1-x86_64.AppImage"));
        }),
        ("home-handoff-and-update-toasts-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 1.2.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_handoff_notice_for(s("/home/me/Apps/MultiCore-1.1.1-x86_64.AppImage"));
        }),
        ("home-service-toast", 840, 720, |ui| {
            ready(ui);
            ui.set_service_card_visible(true);
        }),
        ("home-three-toasts", 840, 720, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 1.2.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_service_card_visible(true);
            ui.set_handoff_notice_for(s("/home/me/Apps/MultiCore-1.1.1-x86_64.AppImage"));
        }),
        ("home-three-toasts-min", 700, 620, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 1.2.0"));
            ui.set_update_action_label(s("Обновить"));
            ui.set_update_action_enabled(true);
            ui.set_service_card_visible(true);
            ui.set_handoff_notice_for(s("/home/me/Apps/MultiCore-1.1.1-x86_64.AppImage"));
        }),
        ("first-run-no-service-toast", 840, 720, |ui| {
            ui.set_service_card_visible(true);
        }),
        ("home-ready-wide", 1600, 900, ready),
        ("home-update-banner-wide", 1600, 900, |ui| {
            ready(ui);
            ui.set_update_available(true);
            ui.set_update_status(s("MULTICORE 0.4.0"));
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
        ("settings-service-card", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_tun_toggle_available(true);
            ui.set_service_card_visible(true);
        }),
        ("settings-service-card-error-min", 700, 620, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_tun_toggle_available(true);
            ui.set_service_card_visible(true);
            ui.set_service_install_error(s("Установщик не прошёл SHA-256 проверку"));
        }),
        ("settings-service-card-busy", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_tun_toggle_available(true);
            ui.set_service_card_visible(true);
            ui.set_service_install_busy(true);
        }),
        ("settings-updates", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
            ui.set_settings_section(s("updates"));
            ui.set_update_status(s("MULTICORE 0.4.0 доступна"));
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
    let group = |id: &str, label: &str, selected: bool| TrayMenuRow {
        id: s(id),
        group_id: s(""),
        icon: s(""),
        icon_image: Default::default(),
        label: s(label),
        detail: s(""),
        tone: s(""),
        selected,
        enabled: true,
        measuring: false,
    };
    let node = |id: &str, icon: &str, label: &str, latency: &str, tone: &str, selected: bool| {
        TrayMenuRow {
            id: s(id),
            group_id: s("g1"),
            icon: s(icon),
            icon_image: crate::emoji_images::image_for(icon),
            label: s(label),
            detail: s(latency),
            tone: s(tone),
            selected,
            enabled: true,
            measuring: false,
        }
    };
    (
        ModelRc::new(VecModel::from(vec![
            group("g1", "Серверы", true),
            group("g2", "Игры", false),
            group("g3", "Медиа", false),
        ])),
        ModelRc::new(VecModel::from(vec![
            node("n1", "🌍", "Автовыбор", "42 мс", "success", true),
            node("n2", "🇸🇪", "Sweden", "42 мс", "success", false),
            node("n3", "🇩🇪", "Germany", "96 мс", "warning", false),
            node("n4", "🇦🇱", "Albania", "Тайм-аут", "danger", false),
        ])),
    )
}

/// Plays a real refresh (pending on, then off with a new quota) with full motion
/// and stacks the title-bar strip at fixed times into one image, top to bottom:
/// before, refreshing, then roughly every 100 ms (wall clock) up to 2.4 s.
fn chip_motion_strip(unlimited: bool) {
    const WIDTH: u32 = 840;
    const HEIGHT: u32 = 720;
    const STRIP: (u32, u32, u32, u32) = (40, 0, 300, 50);
    let ui = AppWindow::new().expect("create app window");
    if unlimited {
        chip(&ui, "∞ GiB", "12 дн.", -1.0, "accent");
    } else {
        chip_normal(&ui);
    }
    ui.set_reduced_motion(false);
    let window = LAST_WINDOW
        .with(|last| last.borrow().clone())
        .expect("preview window adapter");
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    ui.show().expect("show preview component");
    let mut frames = Vec::new();
    let render = |frames: &mut Vec<image::RgbImage>, keep: bool| {
        window.request_redraw();
        let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); (WIDTH * HEIGHT) as usize];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, WIDTH as usize);
        });
        if keep {
            let bytes = buffer
                .iter()
                .flat_map(|pixel| [pixel.r, pixel.g, pixel.b])
                .collect::<Vec<_>>();
            let full = image::RgbImage::from_raw(WIDTH, HEIGHT, bytes).expect("frame");
            frames.push(
                image::imageops::crop_imm(&full, STRIP.0, STRIP.1, STRIP.2, STRIP.3).to_image(),
            );
        }
    };
    let tick = |ms: u64| {
        std::thread::sleep(Duration::from_millis(ms));
        slint::platform::update_timers_and_animations();
    };
    tick(16);
    render(&mut frames, true);
    ui.set_subscription_refresh_pending(true);
    for _ in 0..10 {
        tick(16);
        render(&mut frames, false);
    }
    render(&mut frames, true);
    if !unlimited {
        ui.set_subscription_remaining(s("86.4 GiB"));
        ui.set_subscription_used_fraction(0.136);
    }
    ui.set_subscription_refresh_pending(false);
    // Real time: a software frame takes tens of ms, and Slint's timers and
    // animations run on the wall clock.
    let started = std::time::Instant::now();
    let mut next_keep = 0;
    while started.elapsed() <= Duration::from_millis(2_400) {
        tick(16);
        let elapsed = started.elapsed().as_millis();
        let keep = elapsed >= next_keep;
        if keep {
            next_keep += 100;
        }
        render(&mut frames, keep);
    }
    ui.hide().expect("hide preview component");
    let scale = 2;
    let mut sheet = image::RgbImage::from_pixel(
        STRIP.2 * scale,
        frames.len() as u32 * (STRIP.3 * scale + 4),
        image::Rgb([60, 0, 60]),
    );
    for (index, frame) in frames.iter().enumerate() {
        let big = image::imageops::resize(
            frame,
            STRIP.2 * scale,
            STRIP.3 * scale,
            image::imageops::FilterType::Nearest,
        );
        image::imageops::replace(
            &mut sheet,
            &big,
            0,
            index as i64 * i64::from(STRIP.3 * scale + 4),
        );
    }
    let directory = output_dir();
    std::fs::create_dir_all(&directory).expect("create preview directory");
    let name = if unlimited {
        "chip-motion-strip-unlimited.png"
    } else {
        "chip-motion-strip.png"
    };
    let path = directory.join(name);
    sheet.save(&path).expect("save motion strip");
    println!("{}", path.display());
}

fn tray_many_rows() -> ModelRc<TrayMenuRow> {
    let rows = [
        ("n1", "🌍", "Автовыбор", "42 мс", "success", true),
        ("n2", "🇸🇪", "Sweden", "42 мс", "success", false),
        ("n3", "🇩🇪", "Germany", "96 мс", "warning", false),
        ("n4", "🇳🇱", "Netherlands", "48 мс", "success", false),
        ("n5", "🇫🇮", "Finland", "52 мс", "success", false),
        ("n6", "🇵🇱", "Poland", "61 мс", "success", false),
        ("n7", "🇺🇸", "United States", "144 мс", "warning", false),
        ("n8", "🇯🇵", "Japan", "220 мс", "warning", false),
        ("n9", "🇦🇱", "Albania", "Тайм-аут", "danger", false),
    ]
    .into_iter()
    .map(|(id, icon, label, latency, tone, selected)| TrayMenuRow {
        id: s(id),
        group_id: s("g1"),
        icon: s(icon),
        icon_image: crate::emoji_images::image_for(icon),
        label: s(label),
        detail: s(latency),
        tone: s(tone),
        selected,
        enabled: true,
        measuring: false,
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn tray_connected(popup: &TrayPopup) {
    let (groups, nodes) = tray_rows();
    popup.set_status_text(s("В сети · Автовыбор"));
    popup.set_connection_label(s("Отключиться"));
    popup.set_connected(true);
    popup.set_group_rows(groups);
    popup.set_node_rows(nodes);
    popup.set_subscription_title(s("Fjord VPN"));
    popup.set_subscription_remaining(s("87.6 GiB"));
    popup.set_subscription_days(s("12 дн."));
    popup.set_subscription_used_fraction(0.124);
    popup.set_subscription_unlimited(false);
    popup.set_toggle_title(s("Защита включена"));
    popup.set_toggle_on(true);
    popup.set_status_tone(s("success"));
    popup.set_exit_name(s("Sweden"));
    popup.set_exit_icon_image(crate::emoji_images::image_for("🇸🇪"));
    popup.set_exit_latency(s("42 мс"));
    popup.set_session_uptime(s("00:12:43"));
}

#[test]
#[ignore = "writes preview screenshots; run explicitly with --features preview-render"]
fn render_ui_previews() {
    slint::platform::set_platform(Box::new(PreviewPlatform)).expect("install preview platform");
    for (name, width, height, setup) in scenarios() {
        if !wanted(name) {
            continue;
        }
        let ui = AppWindow::new().expect("create app window");
        setup(&ui);
        capture(&ui, name, width, height);
    }

    // Motion frame 2: the provider logo right after it popped in (before the
    // 350 ms phase timer hands over to the hold).
    let ui = AppWindow::new().expect("create app window");
    chip(&ui, "86.4 GiB", "12 дн.", 0.136, "accent");
    ui.set_chip_phase(s("logo"));
    capture_after(&ui, "chip-motion-2-logo", 840, 720, 1);

    if wanted("chip-motion-strip") {
        chip_motion_strip(false);
        chip_motion_strip(true);
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

    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    capture(&popup, "tray-connected", 304, 460);

    // Dragged larger by its top-left corner: only the accent edges light up (no size readout).
    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    popup.set_node_rows(tray_many_rows());
    popup.set_resize_active(s("c"));
    capture(&popup, "tray-resizing", 400, 640);

    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    popup.set_node_rows(tray_many_rows());
    capture(&popup, "tray-large", 400, 640);

    // No traffic limit: infinite traffic, no usage bar.
    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    popup.set_subscription_remaining(s("∞ GiB"));
    popup.set_subscription_days(s("Бессрочно"));
    popup.set_subscription_unlimited(true);
    capture(&popup, "tray-unlimited", 304, 460);

    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    popup.set_toggle_title(s("Подключение…"));
    popup.set_toggle_busy(true);
    popup.set_status_tone(s("neutral"));
    popup.set_session_uptime(s(""));
    popup.set_reduced_motion(true);
    capture(&popup, "tray-connecting", 304, 460);

    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    popup.set_toggle_title(s("Защита выключена"));
    popup.set_toggle_on(false);
    popup.set_status_tone(s("neutral"));
    popup.set_session_uptime(s(""));
    popup.set_exit_name(s("Автовыбор"));
    popup.set_exit_icon_image(crate::emoji_images::image_for("🌍"));
    popup.set_connection_label(s("Подключиться"));
    capture(&popup, "tray-off", 304, 460);

    let popup = TrayPopup::new().expect("create tray popup");
    tray_connected(&popup);
    popup.set_toggle_title(s("Не удалось подключиться"));
    popup.set_toggle_on(false);
    popup.set_status_tone(s("danger"));
    popup.set_toggle_detail_is_exit(false);
    popup.set_supporting(s("Сервер не ответил за 15 с"));
    popup.set_subscription_failed(true);
    capture(&popup, "tray-error", 304, 460);

    let popup = TrayPopup::new().expect("create tray popup");
    popup.set_status_text(s("Профиль не добавлен"));
    popup.set_connection_label(s("Подключиться"));
    popup.set_connection_enabled(false);
    popup.set_has_profile(false);
    capture(&popup, "tray-no-profile", 304, 460);

    v112_previews();
}

// ---- 1.1.2: scrolling group tabs, image group icons, dumb mode, per-row latency, crisp logos.

fn preview_scale() -> f32 {
    std::env::var("MULTICORE_PREVIEW_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

/// Provider-style group icons (Mihomo `icon:` URLs serve images like these), normalized by the
/// same code the daemon runs on a downloaded icon.
const V112_ICONS: &[(&str, &str)] = &[
    (
        "icon-ru",
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect x='1' y='4' width='22' height='16' rx='3' fill='#ffffff'/><rect x='1' y='9.33' width='22' height='5.34' fill='#1c57d8'/><path d='M1 14.67h22V17a3 3 0 0 1-3 3H4a3 3 0 0 1-3-3z' fill='#e0312b'/></svg>",
    ),
    (
        "icon-world",
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><circle cx='12' cy='12' r='10' fill='#2f80ed'/><path d='M12 2a10 10 0 0 1 0 20M12 2a10 10 0 0 0 0 20M2 12h20M4.5 6.5h15M4.5 17.5h15' stroke='#d6e8ff' stroke-width='1.4' fill='none'/><path d='M12 2c3 3 3 17 0 20M12 2c-3 3-3 17 0 20' stroke='#d6e8ff' stroke-width='1.4' fill='none'/></svg>",
    ),
    (
        "icon-telegram",
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><circle cx='12' cy='12' r='11' fill='#29a9eb'/><path d='M5.5 11.7l11.4-4.4c.6-.2 1 .1.8.9l-1.9 9c-.1.6-.5.8-1 .5l-2.9-2.2-1.4 1.4c-.2.2-.3.3-.6.3l.2-3 5.4-4.9c.2-.2 0-.3-.3-.1l-6.7 4.2-2.9-.9c-.6-.2-.6-.6.2-.8z' fill='#ffffff'/></svg>",
    ),
    (
        "icon-youtube",
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect x='1' y='4.5' width='22' height='15' rx='4.5' fill='#ff0033'/><path d='M10 8.6v6.8l5.8-3.4z' fill='#ffffff'/></svg>",
    ),
    (
        "icon-discord",
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><rect x='1' y='1' width='22' height='22' rx='6' fill='#5865f2'/><path d='M7 8.2c1.3-.7 2.6-1 3.4-1.1l.3.6a10 10 0 0 1 2.6 0l.3-.6c.8.1 2.1.4 3.4 1.1 1.3 2.1 1.9 4.3 1.7 6.6a8.8 8.8 0 0 1-3 1.5l-.6-1c.5-.2 1-.4 1.4-.7l-.4-.3a7.6 7.6 0 0 1-6.2 0l-.4.3c.4.3.9.5 1.4.7l-.6 1a8.8 8.8 0 0 1-3-1.5C5.1 12.5 5.7 10.3 7 8.2z' fill='#ffffff'/><circle cx='9.8' cy='12.3' r='1.1' fill='#5865f2'/><circle cx='14.2' cy='12.3' r='1.1' fill='#5865f2'/></svg>",
    ),
    (
        "icon-auto",
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'><circle cx='12' cy='12' r='11' fill='#1f9d55'/><path d='M7 13l3 3 7-8' stroke='#ffffff' stroke-width='2.4' fill='none' stroke-linecap='round' stroke-linejoin='round'/></svg>",
    ),
];

fn install_v112_icons() {
    for (id, svg) in V112_ICONS {
        let png = multicore_core::normalize_group_icon(svg.as_bytes()).expect("preview icon");
        assert!(crate::crisp_image::store_group_icon(id, &png));
    }
}

fn v112_icon(id: &str, logical: f32) -> slint::Image {
    crate::crisp_image::group_icon(id, logical, preview_scale()).expect("stored preview icon")
}

/// (id, emoji, image icon id, label)
const V112_GROUPS: &[(&str, &str, &str, &str)] = &[
    ("g1", "🌍", "", "Сервер"),
    ("g2", "", "icon-ru", "Россия"),
    ("g3", "", "icon-world", "Зарубежье"),
    ("g4", "", "icon-telegram", "Telegram"),
    ("g5", "", "icon-youtube", "YouTube"),
    ("g6", "🎮", "", "Игры"),
    ("g7", "", "icon-discord", "Discord"),
    ("g8", "📺", "", "Стриминг"),
    ("g9", "", "", "Прочее"),
];

fn v112_groups(count: usize, selected: &str) -> ModelRc<UiCatalogGroup> {
    let rows = V112_GROUPS
        .iter()
        .take(count)
        .map(|(id, emoji, image, label)| {
            let icon = if emoji.is_empty() { "◎" } else { emoji };
            UiCatalogGroup {
                id: s(id),
                icon: s(icon),
                icon_image: if image.is_empty() {
                    crate::emoji_images::image_for(icon)
                } else {
                    v112_icon(image, 15.0)
                },
                label: s(label),
                selected: *id == selected,
            }
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn v112_nodes(measuring: &[&str]) -> ModelRc<UiCatalogNode> {
    let rows = [
        ("n1", "", "icon-auto", "Автовыбор", true, "42 мс", "success"),
        ("n2", "🇦🇱", "", "Albania", false, "96 мс", "warning"),
        ("n3", "🇩🇪", "", "Germany", false, "181 мс", "danger"),
        ("n4", "🇸🇪", "", "Sweden", false, "Тайм-аут", "danger"),
        ("n5", "🇳🇱", "", "Netherlands", false, "64 мс", "success"),
        ("n6", "", "", "Без VPN", false, "—", "neutral"),
    ]
    .into_iter()
    .map(|(id, emoji, image, label, selected, latency, tone)| {
        let busy = measuring.contains(&id);
        UiCatalogNode {
            id: s(id),
            icon: s(emoji),
            icon_image: if image.is_empty() {
                crate::emoji_images::image_for(emoji)
            } else {
                v112_icon(image, 22.0)
            },
            label: s(label),
            selected,
            latency_text: s(if busy { "" } else { latency }),
            latency_tone: s(if busy { "neutral" } else { tone }),
            measuring: busy,
        }
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn v112_home(ui: &AppWindow, groups: usize, selected: &str) {
    connected(ui);
    ui.set_catalog_groups(v112_groups(groups, selected));
    ui.set_catalog_nodes(v112_nodes(&[]));
    ui.set_node_icon(s("◎"));
    ui.set_node_icon_image(v112_icon("icon-auto", 14.0));
}

/// A provider logo with fine detail (thin ring, small mark), 256 px like the daemon caches it.
const V112_LOGO_SVG: &str = "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 64 64'><rect width='64' height='64' rx='14' fill='#0f1720'/><circle cx='32' cy='32' r='22' fill='none' stroke='#4c9dff' stroke-width='4'/><path d='M22 40V24l10 10 10-10v16' fill='none' stroke='#ffffff' stroke-width='5' stroke-linejoin='round' stroke-linecap='round'/></svg>";

fn v112_logo_source() -> image::RgbaImage {
    let png = multicore_core::normalize_service_logo(V112_LOGO_SVG.as_bytes()).expect("logo");
    crate::crisp_image::decode_png(&png).expect("decoded logo")
}

fn v112_chip_logo(ui: &AppWindow, crisp: bool) {
    chip(ui, "∞ GiB", "Бессрочно", -1.0, "accent");
    let source = v112_logo_source();
    let image = if crisp {
        crate::crisp_image::to_slint(&crate::crisp_image::fit(
            &source,
            crate::crisp_image::physical_px(12.0, preview_scale()),
        ))
    } else {
        crate::crisp_image::to_slint(&source)
    };
    ui.set_has_service_logo(true);
    ui.set_service_logo(image);
}

fn v112_tray_groups(count: usize, selected: &str) -> ModelRc<TrayMenuRow> {
    let rows = V112_GROUPS
        .iter()
        .take(count)
        .map(|(id, emoji, image, label)| TrayMenuRow {
            id: s(id),
            group_id: s(""),
            icon: s(emoji),
            icon_image: if image.is_empty() {
                crate::emoji_images::image_for(emoji)
            } else {
                v112_icon(image, 14.0)
            },
            label: s(label),
            detail: s(""),
            tone: s(""),
            selected: *id == selected,
            enabled: true,
            measuring: false,
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn v112_tray_nodes(measuring: &[&str]) -> ModelRc<TrayMenuRow> {
    let rows = [
        ("n1", "", "icon-auto", "Автовыбор", "42 мс", "success", true),
        ("n2", "🇸🇪", "", "Sweden", "42 мс", "success", false),
        ("n3", "🇩🇪", "", "Germany", "96 мс", "warning", false),
        ("n4", "🇦🇱", "", "Albania", "Тайм-аут", "danger", false),
    ]
    .into_iter()
    .map(|(id, emoji, image, label, latency, tone, selected)| {
        let busy = measuring.contains(&id);
        TrayMenuRow {
            id: s(id),
            group_id: s("g1"),
            icon: s(emoji),
            icon_image: if image.is_empty() {
                crate::emoji_images::image_for(emoji)
            } else {
                v112_icon(image, 20.0)
            },
            label: s(label),
            detail: s(if busy { "" } else { latency }),
            tone: s(if busy { "neutral" } else { tone }),
            selected,
            enabled: true,
            measuring: busy,
        }
    })
    .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn v112_previews() {
    install_v112_icons();
    type Setup = fn(&AppWindow);
    let home: &[(&str, u32, u32, Setup)] = &[
        ("v112-tabs-fit-icons", 840, 720, |ui| v112_home(ui, 4, "g1")),
        ("v112-tabs-overflow", 840, 720, |ui| v112_home(ui, 9, "g1")),
        ("v112-tabs-overflow-scrolled", 840, 720, |ui| {
            v112_home(ui, 9, "g7")
        }),
        ("v112-tabs-overflow-end", 840, 720, |ui| {
            v112_home(ui, 9, "g9")
        }),
        ("v112-tabs-overflow-min", 700, 620, |ui| {
            v112_home(ui, 9, "g1")
        }),
        ("v112-tabs-overflow-scrolled-min", 700, 620, |ui| {
            v112_home(ui, 9, "g5")
        }),
        ("v112-tabs-long-name-min", 700, 620, |ui| {
            v112_home(ui, 3, "g1");
            use slint::Model as _;
            let mut rows: Vec<UiCatalogGroup> = ui.get_catalog_groups().iter().collect();
            rows[2].label = s("Очень длинное название группы от провайдера подписки");
            ui.set_catalog_groups(ModelRc::new(VecModel::from(rows)));
        }),
        ("v112-node-measuring", 840, 720, |ui| {
            v112_home(ui, 4, "g1");
            ui.set_catalog_nodes(v112_nodes(&["n3", "n5"]));
        }),
        ("v112-dumb-mode", 840, 720, |ui| {
            v112_home(ui, 1, "g1");
            ui.set_catalog_dumb_mode(true);
        }),
        ("v112-dumb-mode-min", 700, 620, |ui| {
            v112_home(ui, 1, "g1");
            ui.set_catalog_dumb_mode(true);
        }),
        ("v112-chip-logo-crisp", 840, 720, |ui| {
            v112_chip_logo(ui, true)
        }),
        ("v112-chip-logo-naive", 840, 720, |ui| {
            v112_chip_logo(ui, false)
        }),
    ];
    for (name, width, height, setup) in home {
        if !wanted(name) {
            continue;
        }
        let ui = AppWindow::new().expect("create app window");
        setup(&ui);
        capture(&ui, name, *width, *height);
    }

    let tray: &[(&str, &str, bool, &[&str])] = &[
        ("v112-tray-overflow", "g1", false, &[]),
        ("v112-tray-overflow-scrolled", "g7", false, &[]),
        ("v112-tray-measuring", "g1", false, &["n3"]),
        ("v112-tray-dumb-mode", "g1", true, &[]),
    ];
    for (name, selected, dumb, measuring) in tray {
        if !wanted(name) {
            continue;
        }
        let popup = TrayPopup::new().expect("create tray popup");
        tray_connected(&popup);
        popup.set_reduced_motion(true);
        popup.set_group_rows(v112_tray_groups(if *dumb { 1 } else { 9 }, selected));
        popup.set_node_rows(v112_tray_nodes(measuring));
        popup.set_dumb_mode(*dumb);
        popup.set_exit_name(s("Автовыбор"));
        popup.set_exit_icon_image(v112_icon("icon-auto", 12.0));
        capture(&popup, name, 304, 460);
    }
}
