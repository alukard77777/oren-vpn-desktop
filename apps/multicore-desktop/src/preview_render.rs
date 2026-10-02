//! Offscreen UI previews rendered with Slint's software renderer.
//!
//! Run with:
//! `cargo test -p multicore-desktop --features preview-render -- --ignored render_ui_previews`
//!
//! Screenshots are written to `artifacts/screenshots/<MULTICORE_PREVIEW_SET or "current">`.
//! `MULTICORE_PREVIEW_SCALE=1.25` renders at a display scale factor (pixel-alignment checks).
//! `MULTICORE_PREVIEW_LANG=en` renders the English interface (Russian otherwise).
//! No window, GPU, daemon, or user data directory is touched.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{PlatformError, WindowAdapter};
use slint::{ComponentHandle, Model, ModelRc, PhysicalSize, Rgb8Pixel, SharedString, VecModel};

use crate::{
    AppWindow, TrayMenuRow, TrayPopup, UiCatalogGroup, UiCatalogNode, UiEvent, UiProxyMapping,
    UiRouteSearchHit, UiSubscriptionProfile,
};

thread_local! {
    static LAST_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
    /// Time skipped ahead by `advance_clock`, on top of the real clock.
    static CLOCK_SKIP: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

static CLOCK_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

struct PreviewPlatform;

impl slint::platform::Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        LAST_WINDOW.with(|last| *last.borrow_mut() = Some(window.clone()));
        Ok(window)
    }

    fn duration_since_start(&self) -> Duration {
        CLOCK_START.get_or_init(std::time::Instant::now).elapsed() + CLOCK_SKIP.get()
    }
}

/// Moves Slint's clock forward in 50 ms frames (timers fire, animations finish) without
/// waiting for real time to pass.
fn advance_clock(total: Duration) {
    let frame = Duration::from_millis(50);
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(frame);
        CLOCK_SKIP.set(CLOCK_SKIP.get() + step);
        slint::platform::update_timers_and_animations();
        left -= step;
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

/// `MULTICORE_PREVIEW_LANG=en`: the interface language of the previews (Russian otherwise).
fn preview_language() -> multicore_i18n::Lang {
    std::env::var("MULTICORE_PREVIEW_LANG")
        .ok()
        .and_then(|tag| multicore_i18n::Lang::from_tag(&tag))
        .unwrap_or_default()
}

/// A main window in the preview language. Slint picks a bundled translation from the system
/// locale when the first component is created, so the language is selected every time.
fn new_app() -> AppWindow {
    let ui = AppWindow::new().expect("create app window");
    let language = preview_language();
    slint::select_bundled_translation(language.slint_bundle())
        .expect("select the preview language");
    ui.set_language(language.code().into());
    ui
}

/// A tray popup in the preview language (see [`new_app`]).
fn new_popup() -> TrayPopup {
    let popup = TrayPopup::new().expect("create tray popup");
    slint::select_bundled_translation(preview_language().slint_bundle())
        .expect("select the preview language");
    popup
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

/// Like `capture`, with the pointer resting at `pointer` (logical px) long enough for hover
/// tooltips to appear.
fn capture_hover(
    component: &impl ComponentHandle,
    name: &str,
    width: u32,
    height: u32,
    pointer: (f32, f32),
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
    settle(2);
    window.request_redraw();
    let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); (width * height) as usize];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, width as usize);
    });
    window.dispatch_event(slint::platform::WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(pointer.0, pointer.1),
    });
    // 400 ms delay plus the fade.
    settle(6);
    window.request_redraw();
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

/// Like `capture`, after a long wheel scroll at `pointer` (logical px): the page lands on its
/// end, with the overlay scrollbar still showing from the movement.
fn capture_scrolled(
    component: &impl ComponentHandle,
    name: &str,
    width: u32,
    height: u32,
    pointer: (f32, f32),
) {
    if !wanted(name) {
        return;
    }
    let window = LAST_WINDOW
        .with(|last| last.borrow().clone())
        .expect("preview window adapter");
    let scale = preview_scale();
    let (width, height) = (
        (width as f32 * scale) as u32,
        (height as f32 * scale) as u32,
    );
    window.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    window.set_size(PhysicalSize::new(width, height));
    component.show().expect("show preview component");
    settle(3);
    let position = slint::LogicalPosition::new(pointer.0, pointer.1);
    window.dispatch_event(slint::platform::WindowEvent::PointerMoved { position });
    window.dispatch_event(slint::platform::WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y: -40_000.0,
    });
    settle(2);
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
        icon_loading: false,
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
        icon_loading: false,
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
                quota_tooltip: s(if expired {
                    "Подписка истекла 30 сент. · трафик 100 / 100 GiB"
                } else if used < 0.0 {
                    "Трафик без лимита · бессрочно"
                } else {
                    "Трафик: 12.4 / 100 GiB · осталось 87.6 GiB · до 14 окт."
                }),
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
    ui.set_subscription_quota_tooltip(s(if tone == "danger" {
        "Подписка истекла 30 сент. · трафик 60 / 100 GiB"
    } else if used < 0.0 {
        "Трафик без лимита · бессрочно"
    } else {
        "Трафик: 12.4 / 100 GiB · осталось 87.6 GiB · до 14 окт."
    }));
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
            icon_pending: false,
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

fn finding(tone: &str, title: &str, detail: &str) -> multicore_core::troubleshoot::Finding {
    use multicore_core::troubleshoot::Tone;
    let tone = match tone {
        "error" => Tone::Error,
        "warning" => Tone::Warning,
        "success" => Tone::Success,
        _ => Tone::Info,
    };
    multicore_core::troubleshoot::Finding::new(tone, title, detail)
}

fn winsock_offer() -> multicore_core::troubleshoot::FixOffer {
    multicore_core::troubleshoot::FixOffer {
        id: "winsock-reset".into(),
        title: "Сбросить сетевые настройки (Winsock)".into(),
        detail: "Восстанавливает сетевой стек Windows, если его испортила другая программа. Нужна перезагрузка ПК; некоторые VPN и антивирусы после сброса придётся переустановить.".into(),
        needs_admin: true,
        needs_reboot: true,
        title_key: None,
        detail_key: None,
    }
}

/// The copy the daemon writes for a machine with Kaspersky, zapret, a busy port, a foreign proxy
/// and Drover (see crates/multicore-daemon/src/troubleshoot).
fn troubleshoot_mixed_run() -> crate::troubleshoot::TroubleshootRun {
    use multicore_core::troubleshoot::{FixOffer, StepReport};
    let mut run = crate::troubleshoot::TroubleshootRun::default();
    run.add_step(StepReport {
        findings: vec![finding("error", "Обнаружен Kaspersky", "Kaspersky часто блокирует VPN. Если подключение не работает — выключите его или добавьте MULTICORE в исключения.")],
        fixes: Vec::new(),
    });
    run.add_step(StepReport {
        findings: vec![
            finding("error", "Работает zapret", "Программа обхода блокировок перехватывает трафик и мешает VPN."),
            finding("warning", "Найдены программы обхода блокировок", "C:\\Users\\user\\Downloads\\zapret. Если они запущены — закройте их. MULTICORE эти папки не трогает."),
        ],
        fixes: vec![FixOffer {
            id: "remove-dpi-service:zapret".into(),
            title: "Остановить и удалить службу zapret".into(),
            detail: "Служба будет остановлена и удалена из Windows. Файлы программы останутся — при необходимости её можно установить снова.".into(),
            needs_admin: true,
            needs_reboot: false,
            title_key: None,
            detail_key: None,
        }],
    });
    run.add_step(StepReport {
        findings: vec![finding("warning", "Порт 10808 занят", "Его занимает «v2rayN.exe», а он нужен подписке. Закройте эту программу и повторите подключение.")],
        fixes: Vec::new(),
    });
    run.add_step(StepReport {
        findings: vec![finding("info", "Системный прокси отключён", "Был включён прокси 127.0.0.1:8080 — он мешает подключению. Включить снова: Параметры → Сеть и Интернет → Прокси.")],
        fixes: Vec::new(),
    });
    run.add_step(StepReport {
        findings: vec![finding("warning", "Найден Drover для Discord", "Подменная version.dll в папке Discord пускает его трафик мимо VPN и может мешать подключению.")],
        fixes: vec![FixOffer {
            id: "delete-drover".into(),
            title: "Удалить Drover из папки Discord".into(),
            detail: "Будут удалены version.dll и drover.ini (файлов: 2). Сам Discord продолжит работать.".into(),
            needs_admin: false,
            needs_reboot: false,
            title_key: None,
            detail_key: None,
        }],
    });
    run.add_step(StepReport {
        findings: vec![finding(
            "info",
            "DNS-кэш очищен",
            "Устаревшие записи DNS больше не помешают подключению.",
        )],
        fixes: vec![winsock_offer()],
    });
    run
}

fn troubleshoot_page(ui: &AppWindow) {
    ready(ui);
    ui.set_troubleshoot_service_reinstall(true);
    ui.set_troubleshoot_return_page(s("home"));
    ui.set_local_page(s("troubleshoot"));
}

fn troubleshoot_home_offer(ui: &AppWindow) {
    ready(ui);
    ui.set_status_eyebrow(s("Не удалось подключиться"));
    ui.set_headline(s("Что-то пошло не так"));
    ui.set_supporting(s(
        "Не удалось запустить Mihomo. Проверьте исполняемые файлы.",
    ));
    ui.set_toggle_title(s("Не удалось подключиться"));
    ui.set_toggle_detail_is_exit(false);
    ui.set_primary_label(s("Проверить снова"));
    ui.set_status_tone(s("danger"));
    ui.set_troubleshoot_offer_visible(true);
}

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
        ("troubleshoot-home-offer", 840, 720, troubleshoot_home_offer),
        ("troubleshoot-start", 840, 720, |ui| {
            troubleshoot_page(ui);
        }),
        ("troubleshoot-start-confirm", 840, 720, |ui| {
            troubleshoot_page(ui);
            ui.set_troubleshoot_repair_confirm(s("wintun"));
        }),
        ("troubleshoot-running", 840, 720, |ui| {
            troubleshoot_page(ui);
            let steps = multicore_core::troubleshoot::CheckStep::for_platform(true);
            crate::present_troubleshoot_steps(ui, &steps, 4);
            ui.set_troubleshoot_phase(s("running"));
        }),
        ("troubleshoot-results", 840, 720, |ui| {
            troubleshoot_page(ui);
            let mut run = troubleshoot_mixed_run();
            run.toggle(0);
            crate::present_troubleshoot_run(ui, &run);
            ui.set_troubleshoot_phase(s("results"));
            ui.set_troubleshoot_checked_ago(s("2 мин назад"));
        }),
        ("troubleshoot-results-min", 700, 620, |ui| {
            troubleshoot_page(ui);
            crate::present_troubleshoot_run(ui, &troubleshoot_mixed_run());
            ui.set_troubleshoot_phase(s("results"));
            ui.set_troubleshoot_checked_ago(s("2 мин назад"));
        }),
        ("troubleshoot-fixed", 840, 720, |ui| {
            troubleshoot_page(ui);
            let mut run = troubleshoot_mixed_run();
            run.toggle(0);
            run.toggle(2);
            let ids = run.checked_ids();
            run.apply_report(
                &ids,
                multicore_core::troubleshoot::FixReport {
                    findings: vec![
                        finding(
                            "success",
                            "Служба zapret удалена",
                            "Если она не успела остановиться, то исчезнет после перезагрузки.",
                        ),
                        finding(
                            "success",
                            "Настройки Winsock сброшены",
                            "Перезагрузите ПК, чтобы изменения вступили в силу.",
                        ),
                    ],
                    reboot_recommended: true,
                },
            );
            crate::present_troubleshoot_run(ui, &run);
            ui.set_troubleshoot_phase(s("results"));
            ui.set_troubleshoot_checked_ago(s("2 мин назад"));
        }),
        ("troubleshoot-clear", 840, 720, |ui| {
            troubleshoot_page(ui);
            let mut run = crate::troubleshoot::TroubleshootRun::default();
            run.add_step(multicore_core::troubleshoot::StepReport {
                findings: vec![finding(
                    "info",
                    "DNS-кэш очищен",
                    "Устаревшие записи DNS больше не помешают подключению.",
                )],
                fixes: vec![winsock_offer()],
            });
            crate::present_troubleshoot_run(ui, &run);
            ui.set_troubleshoot_phase(s("results"));
            ui.set_troubleshoot_checked_ago(s("2 мин назад"));
        }),
        ("troubleshoot-settings", 840, 720, |ui| {
            ready(ui);
            ui.set_local_page(s("settings"));
        }),
        // Opened from Home's failed-connect offer: ✕ and «Повторить подключение» lead back home.
        ("troubleshoot-from-home-min", 700, 620, |ui| {
            troubleshoot_home_offer(ui);
            ui.set_troubleshoot_return_page(s("home"));
            ui.set_local_page(s("troubleshoot"));
        }),
        ("troubleshoot-running-min", 700, 620, |ui| {
            troubleshoot_page(ui);
            let steps = multicore_core::troubleshoot::CheckStep::for_platform(true);
            crate::present_troubleshoot_steps(ui, &steps, 4);
            ui.set_troubleshoot_phase(s("running"));
        }),
        ("troubleshoot-tool-confirm", 840, 720, |ui| {
            troubleshoot_page(ui);
            ui.set_troubleshoot_phase(s("start"));
            ui.set_troubleshoot_repair_confirm(s("service"));
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
        // «Сохранить лог…» feedback: in progress, saved, failed (narrowest window).
        ("status-save-saving-min", 700, 620, |ui| {
            connected(ui);
            ui.set_local_page(s("status"));
            ui.set_diagnostics_save_state(s("saving"));
        }),
        ("status-save-saved-min", 700, 620, |ui| {
            connected(ui);
            ui.set_local_page(s("status"));
            ui.set_diagnostics_save_state(s("saved"));
        }),
        ("status-save-failed-min", 700, 620, |ui| {
            connected(ui);
            ui.set_local_page(s("status"));
            ui.set_diagnostics_save_state(s("failed"));
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
        ("subscriptions-import-kept-connected", 840, 720, |ui| {
            connected(ui);
            ui.set_local_page(s("subscriptions"));
            ui.set_subscription_profiles(profiles(3));
            ui.set_import_notice(s(crate::view_model::IMPORT_KEPT_NOTICE_RU));
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
        icon_loading: false,
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
            icon_loading: false,
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
    let ui = new_app();
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
        icon_loading: false,
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
    // Texts built in Rust (`tr!`) follow the same language on this thread.
    std::mem::forget(multicore_i18n::ThreadLanguage::set(preview_language()));
    for (name, width, height, setup) in scenarios() {
        if !wanted(name) {
            continue;
        }
        let ui = new_app();
        setup(&ui);
        capture(&ui, name, width, height);
    }

    // Motion frame 2: the provider logo right after it popped in (before the
    // 350 ms phase timer hands over to the hold).
    let ui = new_app();
    chip(&ui, "86.4 GiB", "12 дн.", 0.136, "accent");
    ui.set_chip_phase(s("logo"));
    capture_after(&ui, "chip-motion-2-logo", 840, 720, 1);

    if wanted("chip-motion-strip") {
        chip_motion_strip(false);
        chip_motion_strip(true);
    }

    // One window through expand → collapse, like a user toggling the rail while
    // the window grows and shrinks with it: the list must reflow both ways.
    let ui = new_app();
    connected(&ui);
    ui.set_navigation_collapsed(false);
    capture(&ui, "rail-sequence-1-expanded", 1016, 720);
    ui.set_navigation_collapsed(true);
    capture(&ui, "rail-sequence-2-collapsed", 840, 720);

    // Real-world exit line (long node + group + speeds) at the minimum window size
    // must elide inside the card instead of widening the layout past the window.
    let ui = new_app();
    connected(&ui);
    ui.set_node_name(s("Auto Sweden"));
    ui.set_exit_now_name(s("Sweden [se]"));
    ui.set_exit_now_icon_image(crate::emoji_images::image_for("🇸🇪"));
    ui.set_node_latency(s("94 ms"));
    ui.set_traffic_down(s("1016 Б/с"));
    ui.set_traffic_up(s("0 Б/с"));
    ui.set_session_uptime(s("00:00:32"));
    capture(&ui, "home-long-exit-min", 700, 620);

    // Quota tooltips: the pointer rests on the chip's ring, its quota line, a dropdown row's
    // quota column, and the chip at the narrowest window.
    let ui = new_app();
    chip_normal(&ui);
    capture_hover(&ui, "chip-tooltip-ring", 840, 720, (71.0, 19.0));
    let ui = new_app();
    chip(&ui, "∞ GiB", "Бессрочно", -1.0, "accent");
    capture_hover(&ui, "chip-tooltip-unlimited-meta", 840, 720, (190.0, 19.0));
    let ui = new_app();
    chip(&ui, "40.0 GiB", "Срок истёк", 0.6, "danger");
    capture_hover(&ui, "chip-tooltip-expired-min", 700, 620, (71.0, 19.0));
    let ui = new_app();
    chip_normal(&ui);
    ui.set_profile_switch_locked(false);
    ui.set_subscription_profiles(profiles(5));
    ui.set_active_panel(s("subscription"));
    capture_hover(&ui, "chip-tooltip-menu-row", 840, 720, (350.0, 145.0));
    let ui = new_app();
    chip_normal(&ui);
    ui.set_profile_switch_locked(false);
    ui.set_subscription_profiles(profiles(5));
    ui.set_active_panel(s("subscription"));
    capture_hover(&ui, "chip-tooltip-menu-ring", 840, 720, (82.0, 185.0));

    // A subscription switch opens the new notice and hides it again after 15 s.
    for (name, steps) in [
        ("home-announcement-switch-1-shown", 8),
        ("home-announcement-switch-2-hidden", 140),
    ] {
        if wanted(name) {
            let ui = new_app();
            ready(&ui);
            ui.set_reduced_motion(false);
            ui.set_subscription_announcement_text(s(
                "Новые серверы в Финляндии и Польше уже доступны в группе «Сервер».",
            ));
            ui.set_announcement_auto_hide(true);
            ui.set_announcement_open(true);
            capture_after(&ui, name, 840, 720, steps);
        }
    }

    let popup = new_popup();
    tray_connected(&popup);
    capture(&popup, "tray-connected", 304, 460);

    // Dragged larger by its top-left corner: only the accent edges light up (no size readout).
    let popup = new_popup();
    tray_connected(&popup);
    popup.set_node_rows(tray_many_rows());
    popup.set_resize_active(s("c"));
    capture(&popup, "tray-resizing", 400, 640);

    let popup = new_popup();
    tray_connected(&popup);
    popup.set_node_rows(tray_many_rows());
    capture(&popup, "tray-large", 400, 640);

    // No traffic limit: infinite traffic, no usage bar.
    let popup = new_popup();
    tray_connected(&popup);
    popup.set_subscription_remaining(s("∞ GiB"));
    popup.set_subscription_days(s("Бессрочно"));
    popup.set_subscription_unlimited(true);
    capture(&popup, "tray-unlimited", 304, 460);

    let popup = new_popup();
    tray_connected(&popup);
    popup.set_toggle_title(s("Подключение…"));
    popup.set_toggle_busy(true);
    popup.set_status_tone(s("neutral"));
    popup.set_session_uptime(s(""));
    popup.set_reduced_motion(true);
    capture(&popup, "tray-connecting", 304, 460);

    let popup = new_popup();
    tray_connected(&popup);
    popup.set_toggle_title(s("Защита выключена"));
    popup.set_toggle_on(false);
    popup.set_status_tone(s("neutral"));
    popup.set_session_uptime(s(""));
    popup.set_exit_name(s("Автовыбор"));
    popup.set_exit_icon_image(crate::emoji_images::image_for("🌍"));
    popup.set_connection_label(s("Подключиться"));
    capture(&popup, "tray-off", 304, 460);

    let popup = new_popup();
    tray_connected(&popup);
    popup.set_toggle_title(s("Не удалось подключиться"));
    popup.set_toggle_on(false);
    popup.set_status_tone(s("danger"));
    popup.set_toggle_detail_is_exit(false);
    popup.set_supporting(s("Сервер не ответил за 15 с"));
    popup.set_subscription_failed(true);
    capture(&popup, "tray-error", 304, 460);

    let popup = new_popup();
    popup.set_status_text(s("Профиль не добавлен"));
    popup.set_connection_label(s("Подключиться"));
    popup.set_connection_enabled(false);
    popup.set_has_profile(false);
    capture(&popup, "tray-no-profile", 304, 460);

    v112_previews();
    icon_loading_previews();
    scroll_previews();
}

/// Sixteen subscriptions (the maximum): the five sample rows over and over with unique ids.
fn many_profiles(count: usize) -> ModelRc<UiSubscriptionProfile> {
    let base = profiles(5);
    let rows = (0..count)
        .map(|index| {
            let mut row = base.row_data(index % 5).expect("sample profile");
            row.id = s(&format!("p{}", index + 1));
            row.active = index == 0;
            if index >= 5 {
                row.title = s(&format!("{} {}", row.title, index / 5 + 1));
            }
            row
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn tall_settings(ui: &AppWindow) {
    ready(ui);
    ui.set_local_page(s("settings"));
    ui.set_tun_toggle_available(true);
    ui.set_launch_at_sign_in_error(s(
        "Не удалось изменить автозапуск. Проверьте права доступа к реестру и повторите попытку.",
    ));
    ui.set_preference_error(s(
        "Не удалось сохранить настройки. Изменения применятся только до перезапуска.",
    ));
    ui.set_update_status(s("MULTICORE 0.4.0 доступна"));
    ui.set_update_detail(s(
        "Загрузим подписанный пакет и перезапустим клиент. Активный туннель будет корректно остановлен.",
    ));
    ui.set_update_action_label(s("Обновить"));
    ui.set_update_action_enabled(true);
}

fn tall_troubleshoot(ui: &AppWindow) {
    troubleshoot_page(ui);
    let mut run = troubleshoot_mixed_run();
    run.toggle(0);
    crate::present_troubleshoot_run(ui, &run);
    ui.set_troubleshoot_phase(s("results"));
    ui.set_troubleshoot_checked_ago(s("2 мин назад"));
}

fn tall_status(ui: &AppWindow) {
    connected(ui);
    ui.set_local_page(s("status"));
    ui.set_diagnostic_mappings(mappings());
    ui.set_events(events());
}

fn tall_subscriptions(ui: &AppWindow) {
    ready(ui);
    ui.set_profile_switch_locked(false);
    ui.set_subscription_profiles(many_profiles(16));
    ui.set_local_page(s("subscriptions"));
}

/// Every page taller than the window, at the default and the minimum size: once as opened
/// (top) and once wheel-scrolled to its end.
fn scroll_previews() {
    type Setup = fn(&AppWindow);
    let pages: &[(&str, Setup)] = &[
        ("scroll-settings", tall_settings),
        ("scroll-subscriptions", tall_subscriptions),
        ("scroll-troubleshoot", tall_troubleshoot),
        ("scroll-status", tall_status),
    ];
    for (name, setup) in pages {
        for (suffix, width, height) in [("", 840, 720), ("-min", 700, 620)] {
            let top = format!("{name}{suffix}-top");
            if wanted(&top) {
                let ui = new_app();
                setup(&ui);
                capture(&ui, &top, width, height);
            }
            let end = format!("{name}{suffix}-end");
            if wanted(&end) {
                let ui = new_app();
                setup(&ui);
                capture_scrolled(
                    &ui,
                    &end,
                    width,
                    height,
                    (width as f32 * 0.55, height as f32 * 0.5),
                );
            }
        }
    }
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
                icon_loading: false,
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
            icon_loading: false,
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
            icon_loading: false,
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
            icon_loading: false,
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
        let ui = new_app();
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
        let popup = new_popup();
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

// ---- 1.1.3: group icons still downloading (pulsing placeholder, cross-fade, glyph fallback).

/// Groups and routes whose icon image is still downloading in the loading previews.
const LOADING_GROUPS: &[&str] = &["g3", "g5", "g7"];
const LOADING_NODES: &[&str] = &["n1", "n5"];

/// Marks `ids` as still loading (no image yet), or, with `failed`, as finished without
/// an image (the glyph fallback).
fn mark_loading<T: Clone + 'static>(
    model: ModelRc<T>,
    ids: &[&str],
    failed: bool,
    mut edit: impl FnMut(&mut T) -> &str,
    mut apply: impl FnMut(&mut T, bool),
) -> ModelRc<T> {
    use slint::Model as _;
    let rows = model
        .iter()
        .map(|mut row| {
            let id = edit(&mut row).to_owned();
            if ids.contains(&id.as_str()) {
                apply(&mut row, !failed);
            }
            row
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn loading_groups(count: usize, selected: &str, failed: bool) -> ModelRc<UiCatalogGroup> {
    mark_loading(
        v112_groups(count, selected),
        LOADING_GROUPS,
        failed,
        |row| row.id.as_str(),
        |row, loading| {
            row.icon_image = Default::default();
            row.icon_loading = loading;
        },
    )
}

fn loading_nodes(failed: bool) -> ModelRc<UiCatalogNode> {
    mark_loading(
        v112_nodes(&[]),
        LOADING_NODES,
        failed,
        |row| row.id.as_str(),
        |row, loading| {
            row.icon_image = Default::default();
            row.icon_loading = loading;
            if row.icon.is_empty() {
                row.icon = s("◎");
            }
        },
    )
}

fn loading_tray_groups(selected: &str) -> ModelRc<TrayMenuRow> {
    mark_loading(
        v112_tray_groups(9, selected),
        LOADING_GROUPS,
        false,
        |row| row.id.as_str(),
        |row, loading| {
            row.icon_image = Default::default();
            row.icon_loading = loading;
        },
    )
}

fn loading_tray_nodes() -> ModelRc<TrayMenuRow> {
    mark_loading(
        v112_tray_nodes(&[]),
        LOADING_NODES,
        false,
        |row| row.id.as_str(),
        |row, loading| {
            row.icon_image = Default::default();
            row.icon_loading = loading;
            if row.icon.is_empty() {
                row.icon = s("◎");
            }
        },
    )
}

fn loading_home(ui: &AppWindow, failed: bool) {
    v112_home(ui, 9, "g1");
    ui.set_catalog_groups(loading_groups(9, "g1", failed));
    ui.set_catalog_nodes(loading_nodes(failed));
}

fn loading_tray(popup: &TrayPopup) {
    tray_connected(popup);
    popup.set_group_rows(loading_tray_groups("g1"));
    popup.set_node_rows(loading_tray_nodes());
    popup.set_exit_name(s("Автовыбор"));
    popup.set_exit_icon_image(v112_icon("icon-auto", 12.0));
}

fn icon_loading_previews() {
    install_v112_icons();
    type Setup = fn(&AppWindow);
    let home: &[(&str, u32, u32, Setup)] = &[
        // Full motion: the placeholders pulse (captured at whatever phase the clock is in).
        ("icon-loading-home", 840, 720, |ui| {
            loading_home(ui, false);
            ui.set_reduced_motion(false);
            ui.set_ambient_background_enabled(false);
        }),
        // Reduced motion: static muted circles.
        ("icon-loading-home-reduced", 840, 720, |ui| {
            loading_home(ui, false)
        }),
        ("icon-loading-home-min", 700, 620, |ui| {
            loading_home(ui, false)
        }),
        // Loading ended without an image: the glyph fallback, as before.
        ("icon-loading-home-failed", 840, 720, |ui| {
            loading_home(ui, true)
        }),
        ("icon-loading-search", 840, 720, |ui| {
            v112_home(ui, 4, "g1");
            open_route_search(ui, "ger", false);
            use slint::Model as _;
            let mut hits: Vec<UiRouteSearchHit> = ui.get_route_search_hits().iter().collect();
            hits[0].icon_image = Default::default();
            hits[0].icon_loading = true;
            ui.set_route_search_hits(ModelRc::new(VecModel::from(hits)));
        }),
    ];
    for (name, width, height, setup) in home {
        if !wanted(name) {
            continue;
        }
        let ui = new_app();
        setup(&ui);
        capture(&ui, name, *width, *height);
    }

    for (name, reduced) in [
        ("icon-loading-tray", false),
        ("icon-loading-tray-reduced", true),
    ] {
        if !wanted(name) {
            continue;
        }
        let popup = new_popup();
        loading_tray(&popup);
        popup.set_reduced_motion(reduced);
        capture(&popup, name, 304, 460);
    }
}

/// The placeholder pulse reads `animation-tick()` only while an icon is loading: once the
/// image arrives (the row updated in place, so it cross-fades) the window stops asking for
/// animation frames.
#[test]
fn icon_placeholder_animates_only_while_loading() {
    use slint::Model as _;
    // Another test on this thread may have installed it already.
    let _ = slint::platform::set_platform(Box::new(PreviewPlatform));
    install_v112_icons();
    let popup = new_popup();
    tray_connected(&popup);
    popup.set_reduced_motion(false);
    let nodes = Rc::new(VecModel::from(
        v112_tray_nodes(&[]).iter().collect::<Vec<TrayMenuRow>>(),
    ));
    popup.set_node_rows(ModelRc::from(nodes.clone()));
    let window = LAST_WINDOW
        .with(|last| last.borrow().clone())
        .expect("preview window adapter");
    window.set_size(PhysicalSize::new(304, 460));
    popup.show().expect("show tray popup");
    let mut buffer = vec![Rgb8Pixel::new(0, 0, 0); 304 * 460];
    // Advance the clock and draw, like the event loop does each frame.
    let mut run_frames = |ms: u64| {
        for _ in 0..ms / 40 {
            std::thread::sleep(Duration::from_millis(40));
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut buffer, 304);
            });
        }
    };

    run_frames(480);
    assert!(
        !popup.window().has_active_animations(),
        "a settled popup must not animate"
    );

    let mut row = nodes.row_data(0).expect("first node");
    let icon = row.icon_image.clone();
    row.icon_image = Default::default();
    row.icon_loading = true;
    nodes.set_row_data(0, row.clone());
    run_frames(480);
    assert!(
        popup.window().has_active_animations(),
        "a loading icon pulses"
    );

    row.icon_image = icon;
    row.icon_loading = false;
    nodes.set_row_data(0, row);
    run_frames(80);
    assert!(
        popup.window().has_active_animations(),
        "the arriving icon cross-fades"
    );
    run_frames(480);
    assert!(
        !popup.window().has_active_animations(),
        "once the icon is in, nothing keeps animating"
    );
    popup.hide().expect("hide tray popup");
}

/// Clicks on the home route list while snapshots stream in (status, latency, icon updates
/// every few hundred ms). Replacing the rows' model between a press and its release rebuilt
/// every row and dropped the click; rows updated in place keep it.
#[test]
fn a_route_click_survives_snapshots_that_keep_the_rows() {
    use slint::Model as _;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::cell::Cell;
    let _ = slint::platform::set_platform(Box::new(PreviewPlatform));
    let ui = new_app();
    connected(&ui);
    ui.set_catalog_groups(groups());
    ui.set_catalog_nodes(nodes(false));
    let window = LAST_WINDOW
        .with(|last| last.borrow().clone())
        .expect("preview window adapter");
    window.set_size(PhysicalSize::new(840, 720));
    ui.show().expect("show app window");
    settle(4);
    let clicked = Rc::new(RefCell::new(Vec::<SharedString>::new()));
    let sink = clicked.clone();
    ui.on_select_catalog_node(move |id| sink.borrow_mut().push(id));
    let press = |x: f32, y: f32| {
        let position = slint::LogicalPosition::new(x, y);
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
    };
    let release = |x: f32, y: f32| {
        window.dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(x, y),
            button: PointerEventButton::Left,
        });
        slint::platform::update_timers_and_animations();
    };
    // Find the second route row ("n2") with a plain click.
    let x = 420.0;
    let row_y = Cell::new(None);
    for y in (100..720).step_by(4) {
        press(x, y as f32);
        release(x, y as f32);
        if clicked.borrow().last().is_some_and(|id| id == "n2") {
            row_y.set(Some(y as f32 + 8.0));
            break;
        }
    }
    let y = row_y.get().expect("the n2 route row is clickable");
    clicked.borrow_mut().clear();

    // A snapshot that replaces the model mid-click (1.1.2 behaviour): the click is lost.
    press(x, y);
    ui.set_catalog_nodes(nodes(false));
    release(x, y);
    assert!(clicked.borrow().is_empty(), "a rebuilt row loses the click");

    // The same snapshot applied in place: the click lands.
    press(x, y);
    if let Some(model) = crate::sync_rows(
        ui.get_catalog_nodes(),
        nodes(false).iter().collect(),
        |row| row.id.clone(),
    ) {
        ui.set_catalog_nodes(model);
    }
    release(x, y);
    assert_eq!(clicked.borrow().as_slice(), [SharedString::from("n2")]);
    ui.hide().expect("hide app window");
}

/// An auto-opened provider notice closes itself after 15 s. Only a pointer that really moves
/// over it pauses the countdown, which resumes once the pointer leaves; a cursor resting where
/// the popover appeared does not count.
#[test]
fn an_auto_opened_notice_closes_and_hover_only_pauses_it() {
    let _ = slint::platform::set_platform(Box::new(PreviewPlatform));
    let open_notice = |pointer: Option<(f32, f32)>| {
        let ui = new_app();
        ready(&ui);
        ui.set_reduced_motion(false);
        ui.set_subscription_announcement_text(s(
            "Плановые работы сегодня в 23:00 по московскому времени.",
        ));
        let window = LAST_WINDOW
            .with(|last| last.borrow().clone())
            .expect("preview window adapter");
        window
            .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
        window.set_size(PhysicalSize::new(840, 720));
        ui.show().expect("show app window");
        if let Some((x, y)) = pointer {
            window.dispatch_event(slint::platform::WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x, y),
            });
        }
        ui.set_announcement_auto_hide(true);
        ui.set_announcement_open(true);
        advance_clock(Duration::from_millis(100));
        (ui, window)
    };
    let move_to = |window: &MinimalSoftwareWindow, x: f32, y: f32| {
        window.dispatch_event(slint::platform::WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        });
        advance_clock(Duration::from_millis(50));
    };

    // Nobody touches it: closed after 15 s, not before.
    let (ui, _window) = open_notice(None);
    advance_clock(Duration::from_secs(14));
    assert!(ui.get_announcement_open(), "still open before 15 s");
    advance_clock(Duration::from_millis(1_500));
    assert!(!ui.get_announcement_open(), "closed after 15 s");
    ui.hide().expect("hide");

    // The cursor rests where the popover appears (and the opening motion slides under it).
    let (ui, window) = open_notice(Some((250.0, 120.0)));
    advance_clock(Duration::from_millis(600));
    move_to(&window, 250.0, 120.0);
    move_to(&window, 250.5, 120.0);
    advance_clock(Duration::from_secs(15));
    assert!(!ui.get_announcement_open(), "a resting cursor is no hover");
    ui.hide().expect("hide");

    // A real hover pauses it as long as the pointer stays; leaving resumes the countdown.
    let (ui, window) = open_notice(None);
    advance_clock(Duration::from_secs(10));
    move_to(&window, 240.0, 110.0);
    move_to(&window, 250.0, 120.0);
    move_to(&window, 260.0, 125.0);
    advance_clock(Duration::from_secs(30));
    assert!(ui.get_announcement_open(), "paused while hovered");
    move_to(&window, 600.0, 650.0);
    advance_clock(Duration::from_secs(3));
    assert!(
        ui.get_announcement_open(),
        "resumes with the time it had left"
    );
    advance_clock(Duration::from_secs(3));
    assert!(
        !ui.get_announcement_open(),
        "closed once the remaining time ran out"
    );
    ui.hide().expect("hide");

    // Opened by hand from the bell: it stays.
    let (ui, _window) = open_notice(None);
    ui.set_announcement_open(false);
    advance_clock(Duration::from_millis(100));
    ui.set_announcement_open(true);
    advance_clock(Duration::from_secs(20));
    assert!(
        ui.get_announcement_open(),
        "a notice opened by hand stays open"
    );
    ui.hide().expect("hide");
}
