//! Gives back the GPU renderer of a main window that stays hidden in the tray (Windows).
//!
//! Every Slint window on Windows draws with its own Skia Direct3D 12 device: the swap chain,
//! Skia's GPU resource cache, the driver's heaps and its compiled shaders. Hiding the native
//! window keeps all of that. Measured with a 528-route catalog on an NVIDIA GPU, the main
//! window hidden to the tray still held about 70 MB of private working set, 190 MB of commit
//! and 100 MB of video memory, more after it had been maximized. Destroying the hidden window
//! (Slint's suspend path, which Wayland always takes) frees all of it; Slint recreates the
//! window and its renderer on the next `show()`, which then takes a few hundred milliseconds
//! longer for the new device. So the window is released only after it has stayed hidden for
//! `RELEASE_AFTER`: hiding and reopening it in quick succession stays instant.

use std::cell::Cell;
use std::time::Duration;

use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;

use crate::AppWindow;

/// How long the main window stays hidden in the tray before its renderer is released.
pub(crate) const RELEASE_AFTER: Duration = Duration::from_secs(60);

/// Slint 1.17 destroys a window on `hide()` when this variable is set.
const DESTROY_ON_HIDE: &str = "SLINT_DESTROY_WINDOW_ON_HIDE";

thread_local! {
    /// Bumped on every hide to the tray, so only the latest hide's release runs.
    static HIDE_SERIAL: Cell<u64> = const { Cell::new(0) };
}

/// Called right after the main window was hidden to the tray: releases its renderer once it
/// has stayed hidden for `RELEASE_AFTER`.
pub(crate) fn release_when_left_hidden(ui: &AppWindow) {
    let serial = HIDE_SERIAL.with(|serial| {
        serial.set(serial.get().wrapping_add(1));
        serial.get()
    });
    let weak = ui.as_weak();
    slint::Timer::single_shot(RELEASE_AFTER, move || {
        if HIDE_SERIAL.with(Cell::get) != serial {
            return;
        }
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hidden = ui
            .window()
            .with_winit_window(|window| window.is_visible() == Some(false))
            .unwrap_or(false);
        if ui.get_shell_active() || !hidden {
            return;
        }
        if let Err(error) = destroy_hidden(ui.window()) {
            eprintln!("hidden window renderer not released: {error}");
        }
    });
}

/// Destroys the hidden `window` (native window, renderer and its caches) through Slint, which
/// recreates it with the same size and maximized state on the next `show()`.
fn destroy_hidden(window: &slint::Window) -> Result<(), slint::PlatformError> {
    let size = window.size().to_logical(window.scale_factor());
    let maximized = window.is_maximized();
    let preset = std::env::var_os(DESTROY_ON_HIDE).is_some();
    if !preset {
        // SAFETY: the standard library documents `set_var` and `remove_var` as always safe
        // on Windows, also with other threads running.
        unsafe { std::env::set_var(DESTROY_ON_HIDE, "1") };
    }
    let hidden = window.hide();
    if !preset {
        // SAFETY: as above.
        unsafe { std::env::remove_var(DESTROY_ON_HIDE) };
    }
    hidden?;
    if !window.has_winit_window() {
        // Slint keeps the last size and position for the new window but not a maximized
        // state it already believes in; refresh it like the Wayland path does.
        window.set_size(size);
        if maximized {
            window.set_maximized(false);
            window.set_maximized(true);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quick_hide_and_reopen_keeps_the_renderer() {
        assert!(RELEASE_AFTER >= Duration::from_secs(30));
        assert!(RELEASE_AFTER <= Duration::from_secs(300));
    }

    #[test]
    fn hiding_to_the_tray_schedules_the_release_and_only_hides_natively_at_once() {
        let main = include_str!("main.rs");
        let helper = main
            .split("fn hide_window_to_tray(")
            .nth(1)
            .and_then(|source| source.split("fn apply_tray_popup_model").next())
            .expect("tray hide helper");
        let native = helper
            .split("#[cfg(not(target_os = \"linux\"))]")
            .nth(1)
            .expect("native hide branch");
        let hide = native
            .find("window.set_visible(false)")
            .expect("native hide");
        let release = native
            .find("renderer_release::release_when_left_hidden(ui)")
            .expect("deferred release");
        assert!(hide < release);
        let source = include_str!("renderer_release.rs");
        let release_body = source
            .split("fn destroy_hidden(")
            .nth(1)
            .expect("release body");
        assert!(release_body.contains("std::env::set_var(DESTROY_ON_HIDE"));
        assert!(release_body.contains("window.hide()"));
        assert!(release_body.contains("std::env::remove_var(DESTROY_ON_HIDE)"));
        assert!(release_body.contains("window.set_maximized(true)"));
    }
}
