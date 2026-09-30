use std::cell::RefCell;

use copypasta::{ClipboardContext, ClipboardProvider};

use crate::view_model::MAX_SUBSCRIPTION_URL_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardTextError {
    Empty,
    TooLong,
    Unavailable,
}

impl ClipboardTextError {
    pub(crate) const fn user_message(self) -> &'static str {
        match self {
            Self::Empty => "В буфере обмена нет ссылки",
            Self::TooLong => "Ссылка из буфера слишком длинная",
            Self::Unavailable => "Не удалось прочитать буфер обмена",
        }
    }
}

/// Reads the clipboard the way the window's own text fields do. On a Wayland
/// session `ClipboardContext` only sees the XWayland selection, which is stale
/// or empty while a native Wayland window is focused, so the window's Wayland
/// display is used first and X11 stays the fallback.
pub(crate) fn read_subscription_text(window: &slint::Window) -> Result<String, ClipboardTextError> {
    #[cfg(target_os = "linux")]
    if let Some(contents) = wayland::with_clipboard(window, |clipboard| clipboard.get_contents()) {
        return normalize_subscription_text(
            &contents.map_err(|_| ClipboardTextError::Unavailable)?,
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = window;
    normalize_subscription_text(&with_native_clipboard(|clipboard| {
        clipboard.get_contents()
    })?)
}

/// Used by the "copy diagnostics" action: the caller is responsible for
/// only ever passing already-redacted text (see
/// `DesktopViewModel::diagnostics_export_text`).
pub(crate) fn write_clipboard_text(
    window: &slint::Window,
    text: String,
) -> Result<(), ClipboardTextError> {
    #[cfg(target_os = "linux")]
    let text =
        match wayland::with_clipboard(window, |clipboard| clipboard.set_contents(text.clone())) {
            Some(result) => return result.map_err(|_| ClipboardTextError::Unavailable),
            None => text,
        };
    #[cfg(not(target_os = "linux"))]
    let _ = window;
    with_native_clipboard(|clipboard| clipboard.set_contents(text))
}

thread_local! {
    // On X11 the copying process itself serves the selection for as long as
    // its context lives: a context dropped right after `set_contents` takes
    // the copied text with it. Keep one for the process, like Wayland's.
    static NATIVE_CLIPBOARD: RefCell<Option<ClipboardContext>> = const { RefCell::new(None) };
}

fn with_native_clipboard<R>(
    action: impl FnOnce(
        &mut ClipboardContext,
    ) -> Result<R, Box<dyn std::error::Error + Send + Sync + 'static>>,
) -> Result<R, ClipboardTextError> {
    NATIVE_CLIPBOARD.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = Some(ClipboardContext::new().map_err(|_| ClipboardTextError::Unavailable)?);
        }
        let clipboard = slot.as_mut().ok_or(ClipboardTextError::Unavailable)?;
        action(clipboard).map_err(|_| ClipboardTextError::Unavailable)
    })
}

#[cfg(target_os = "linux")]
mod wayland {
    use std::cell::RefCell;
    use std::ffi::c_void;

    use copypasta::wayland_clipboard::{Clipboard, create_clipboards_from_external};
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

    thread_local! {
        // One clipboard per Wayland connection: creating it spawns a worker
        // thread, so it is kept for the process instead of per paste.
        static CLIPBOARD: RefCell<Option<(usize, Clipboard)>> = const { RefCell::new(None) };
    }

    /// `None` when the window is not on Wayland (X11 session, or no window yet).
    pub(super) fn with_clipboard<R>(
        window: &slint::Window,
        action: impl FnOnce(&mut Clipboard) -> R,
    ) -> Option<R> {
        let display = window
            .with_winit_window(|window| match window.display_handle().ok()?.as_raw() {
                RawDisplayHandle::Wayland(handle) => Some(handle.display.as_ptr()),
                _ => None,
            })
            .flatten()?;
        CLIPBOARD.with_borrow_mut(|slot| {
            if slot
                .as_ref()
                .is_none_or(|(current, _)| *current != display as usize)
            {
                *slot = Some((display as usize, open(display)));
            }
            slot.as_mut().map(|(_, clipboard)| action(clipboard))
        })
    }

    fn open(display: *mut c_void) -> Clipboard {
        // SAFETY: the pointer is the live wl_display of winit's event loop,
        // which Slint keeps open for the rest of the process.
        let (_primary, clipboard) = unsafe { create_clipboards_from_external(display) };
        clipboard
    }
}

fn normalize_subscription_text(contents: &str) -> Result<String, ClipboardTextError> {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        Err(ClipboardTextError::Empty)
    } else if trimmed.len() > MAX_SUBSCRIPTION_URL_BYTES {
        Err(ClipboardTextError::TooLong)
    } else {
        Ok(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_plain_text_before_import() {
        assert_eq!(
            normalize_subscription_text("  https://example.com/sub\r\n"),
            Ok("https://example.com/sub".to_owned())
        );
    }

    #[test]
    fn rejects_empty_clipboard_without_starting_import() {
        assert_eq!(
            normalize_subscription_text(" \r\n\t"),
            Err(ClipboardTextError::Empty)
        );
    }

    #[test]
    fn accepts_text_at_the_subscription_url_byte_limit() {
        let contents = "x".repeat(MAX_SUBSCRIPTION_URL_BYTES);
        assert_eq!(normalize_subscription_text(&contents), Ok(contents));
    }

    #[test]
    fn rejects_text_one_byte_over_the_subscription_url_limit() {
        let contents = "x".repeat(MAX_SUBSCRIPTION_URL_BYTES + 1);
        assert_eq!(
            normalize_subscription_text(&contents),
            Err(ClipboardTextError::TooLong)
        );
    }
}
