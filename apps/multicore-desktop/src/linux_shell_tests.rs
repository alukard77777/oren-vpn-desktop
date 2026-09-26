//! Run only in the disposable Wayland session created by test-linux-shell.sh.
//! No daemon, real preferences, subscriptions, updates, or tunnel is started.
use super::*;
use slint::platform::{PointerEventButton, WindowEvent};
use slint::winit_030::winit;

fn wayland_window(tray_available: bool) -> AppWindow {
    use winit::platform::wayland::EventLoopBuilderExtWayland;
    assert_eq!(
        std::env::var("MULTICORE_ISOLATED_SHELL_TEST").as_deref(),
        Ok("1")
    );
    let mut builder =
        winit::event_loop::EventLoop::<slint::winit_030::SlintEvent>::with_user_event();
    builder.with_wayland();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(builder)
        .select()
        .unwrap();
    let ui = AppWindow::new().unwrap();
    ui.set_ambient_background_enabled(false);
    wire_window_controls(&ui, tray_available);
    ui.show().unwrap();
    ui
}

fn click_title_button(ui: &AppWindow, offset_from_right: f32) {
    let logical_width = ui.window().size().width as f32 / ui.window().scale_factor();
    let position = slint::LogicalPosition::new(logical_width - 6.0 - offset_from_right, 28.0);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

#[test]
#[ignore = "requires the isolated native Wayland compositor in test-linux-shell.sh"]
fn wayland_title_buttons_hide_and_restore_without_exiting() {
    let ui = wayland_window(true);
    let weak = ui.as_weak();
    let phase = Rc::new(Cell::new(0));
    let progress = phase.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let ui = weak.upgrade().unwrap();
        match progress.get() {
            0 | 4 | 8 => {
                assert!(ui.window().is_visible());
                assert!(ui.window().has_winit_window());
                click_title_button(&ui, 110.0); // Actual Slint minimize hit target.
                assert!(!ui.window().is_visible(), "minimize must hide on Wayland");
                assert!(
                    !ui.window().has_winit_window(),
                    "Wayland surface must be destroyed"
                );
            }
            1 | 5 | 9 => {
                // Reaching another timer tick with no windows proves keep-alive.
                assert!(!ui.window().is_visible());
                restore_and_focus(&ui);
                assert!(ui.window().is_visible());
                assert!(ui.window().has_winit_window());
            }
            2 | 6 | 10 => {
                click_title_button(&ui, 22.0); // Actual Slint close hit target.
                assert!(!ui.window().is_visible(), "close must hide on Wayland");
                assert!(!ui.window().has_winit_window());
            }
            3 | 7 => restore_and_focus(&ui),
            11 => slint::quit_event_loop().unwrap(),
            _ => panic!("unexpected timer tick"),
        }
        progress.set(progress.get() + 1);
    });
    slint::run_event_loop_until_quit().unwrap();
    assert_eq!(phase.get(), 12, "event loop exited while hidden");
    ui.hide().unwrap();
}

#[test]
#[ignore = "requires the isolated native Wayland compositor in test-linux-shell.sh"]
fn wayland_close_without_tray_exits() {
    let ui = wayland_window(false);
    let weak = ui.as_weak();
    let clicked = Rc::new(Cell::new(false));
    let clicked_in_timer = clicked.clone();
    Timer::single_shot(Duration::from_millis(250), move || {
        clicked_in_timer.set(true);
        click_title_button(&weak.upgrade().unwrap(), 22.0);
    });
    Timer::single_shot(Duration::from_secs(5), || {
        panic!("close without tray did not exit")
    });
    slint::run_event_loop_until_quit().unwrap();
    assert!(clicked.get());
    ui.hide().unwrap();
}

#[test]
#[ignore = "requires isolated Wayland and SNI menu host from test-linux-shell.sh"]
fn wayland_native_tray_menu_restores_and_exits() {
    let ui = wayland_window(true);
    let marker = std::path::PathBuf::from(std::env::var_os("MULTICORE_TEST_MENU_MARKER").unwrap());
    let model = Arc::new(Mutex::new(DesktopViewModel::new(Arc::new(
        UnavailableDaemonClient::new(DaemonError::new("isolated shell test")),
    ))));
    let writer = Arc::new(Mutex::new(PreferenceWriter::start(None)));
    let tracker = Arc::new(Mutex::new(PersistenceTracker::new(
        AppPreferences::default(),
    )));
    let menu = tray_menu_model(&model.lock().unwrap());
    let menu_source = menu.clone();
    let weak = ui.as_weak();
    let actions = Rc::new(Cell::new(0));
    let seen_actions = actions.clone();
    let _tray = tray::start(
        menu,
        move || menu_source.clone(),
        move |interaction| {
            let TrayInteraction::Command(command) = interaction else {
                panic!("SNI must use the native menu");
            };
            match command {
                TrayCommand::ShowWindow => seen_actions.set(seen_actions.get() + 1),
                TrayCommand::Exit => {
                    assert_eq!(seen_actions.get(), 2);
                    seen_actions.set(3);
                }
                _ => panic!("unexpected command from isolated tray host"),
            }
            handle_tray_command(&weak, &model, command, &writer, &tracker);
        },
    )
    .expect("real AppIndicator must initialize with the isolated host");
    let weak = ui.as_weak();
    let phase = Rc::new(Cell::new(0));
    let progress = phase.clone();
    let seen_actions = actions.clone();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), move || {
        let ui = weak.upgrade().unwrap();
        match progress.get() {
            0 => {
                click_title_button(&ui, 110.0);
                assert!(!ui.window().is_visible());
                std::fs::write(&marker, "restore-one").unwrap();
                progress.set(1);
            }
            1 if seen_actions.get() == 1 => {
                assert!(ui.window().is_visible());
                assert!(ui.window().has_winit_window());
                click_title_button(&ui, 22.0);
                assert!(!ui.window().is_visible());
                std::fs::write(&marker, "restore-two").unwrap();
                progress.set(2);
            }
            2 if seen_actions.get() == 2 => {
                assert!(ui.window().is_visible());
                assert!(ui.window().has_winit_window());
                std::fs::write(&marker, "exit").unwrap();
                progress.set(3);
            }
            _ => {}
        }
    });
    Timer::single_shot(Duration::from_secs(15), || {
        panic!("native tray menu timed out")
    });
    slint::run_event_loop_until_quit().unwrap();
    assert_eq!(phase.get(), 3);
    assert_eq!(actions.get(), 3, "must exit through the real D-Bus menu");
    ui.hide().unwrap();
}
