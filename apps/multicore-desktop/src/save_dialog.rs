//! Native "save as" dialog for «Сохранить лог…».
//!
//! Windows: the system's Explorer-style save dialog (`GetSaveFileNameW`, which shows the same
//! common item dialog as `IFileSaveDialog` without pulling in a COM wrapper crate), modal to the
//! main window. Linux: the desktop's file chooser through xdg-desktop-portal
//! (`org.freedesktop.portal.FileChooser.SaveFile`) over D-Bus; without a portal the archive goes
//! straight to the Downloads folder.
//!
//! Both block the calling thread until the user answers, so they run on a worker thread.

use std::path::PathBuf;

/// What the user picked.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SaveTarget {
    /// The file the user chose in the dialog.
    Chosen(PathBuf),
    /// No dialog could be shown: the archive goes to this file in the Downloads folder.
    #[cfg_attr(windows, allow(dead_code))]
    Fallback(PathBuf),
    /// The user closed the dialog.
    Cancelled,
}

/// The owner window, as a raw handle that can cross threads (Windows `HWND`; unused elsewhere).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OwnerWindow(#[cfg_attr(not(windows), allow(dead_code))] pub(crate) isize);

impl OwnerWindow {
    #[cfg(windows)]
    pub(crate) fn of(window: &slint::Window) -> Self {
        use slint::winit_030::WinitWindowAccessor;
        use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        window
            .with_winit_window(|window| {
                window
                    .window_handle()
                    .ok()
                    .and_then(|handle| match handle.as_raw() {
                        RawWindowHandle::Win32(win32) => Some(Self(win32.hwnd.get())),
                        _ => None,
                    })
            })
            .flatten()
            .unwrap_or_default()
    }

    #[cfg(not(windows))]
    pub(crate) fn of(_window: &slint::Window) -> Self {
        Self(0)
    }
}

/// Asks where to save `default_name`. Blocks until the user answers.
pub(crate) fn choose(owner: OwnerWindow, default_name: &str) -> Result<SaveTarget, String> {
    platform::choose(owner, default_name)
}

/// The Downloads folder (or home) for when no dialog can be shown.
#[cfg_attr(windows, allow(dead_code))]
fn fallback_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let home = home.filter(|path| path.is_absolute())?;
    let downloads = xdg_download_dir(&home).unwrap_or_else(|| home.join("Downloads"));
    Some(if downloads.is_dir() { downloads } else { home })
}

/// `XDG_DOWNLOAD_DIR` from `~/.config/user-dirs.dirs` (localised folder names).
#[cfg_attr(windows, allow(dead_code))]
fn xdg_download_dir(home: &std::path::Path) -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let contents = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    let value = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?
        .trim()
        .trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    path.is_absolute().then_some(path)
}

#[cfg(windows)]
mod platform {
    use super::{OwnerWindow, SaveTarget};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
    };
    use windows_sys::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetSaveFileNameW, OFN_EXPLORER, OFN_HIDEREADONLY, OFN_NOCHANGEDIR,
        OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    pub(super) fn choose(owner: OwnerWindow, default_name: &str) -> Result<SaveTarget, String> {
        // The Explorer-style dialog hosts shell components: an STA on this worker thread.
        let initialized = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        let result = show(owner, default_name);
        if initialized >= 0 {
            unsafe { CoUninitialize() };
        }
        result
    }

    fn show(owner: OwnerWindow, default_name: &str) -> Result<SaveTarget, String> {
        let mut buffer = vec![0_u16; 32 * 1024];
        for (slot, unit) in buffer.iter_mut().zip(default_name.encode_utf16()) {
            *slot = unit;
        }
        // Pairs of display name and pattern, double-NUL terminated.
        let filter: Vec<u16> = "ZIP-архив (*.zip)\0*.zip\0\0".encode_utf16().collect();
        let title = wide("Сохранить лог MULTICORE");
        let extension = wide("zip");
        let mut dialog: OPENFILENAMEW = unsafe { std::mem::zeroed() };
        dialog.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        dialog.hwndOwner = owner.0 as _;
        dialog.lpstrFilter = filter.as_ptr();
        dialog.nFilterIndex = 1;
        dialog.lpstrFile = buffer.as_mut_ptr();
        dialog.nMaxFile = buffer.len() as u32;
        dialog.lpstrTitle = title.as_ptr();
        dialog.lpstrDefExt = extension.as_ptr();
        dialog.Flags = OFN_EXPLORER
            | OFN_OVERWRITEPROMPT
            | OFN_PATHMUSTEXIST
            | OFN_NOCHANGEDIR
            | OFN_HIDEREADONLY;
        if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
            return match unsafe { CommDlgExtendedError() } {
                0 => Ok(SaveTarget::Cancelled),
                code => Err(format!("диалог сохранения не открылся (код {code:#x})")),
            };
        }
        let length = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
        Ok(SaveTarget::Chosen(path))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{OwnerWindow, SaveTarget, fallback_directory};
    use gtk::gio::prelude::FileExt;
    use gtk::{gio, glib};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    /// The user may take their time; a portal that never answers still must not hang the
    /// worker forever.
    const ANSWER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

    pub(super) fn choose(_owner: OwnerWindow, default_name: &str) -> Result<SaveTarget, String> {
        match portal_save_file(default_name) {
            Ok(Some(path)) => Ok(SaveTarget::Chosen(path)),
            Ok(None) => Ok(SaveTarget::Cancelled),
            Err(error) => {
                eprintln!("file chooser portal unavailable ({error}); saving to Downloads");
                fallback_directory()
                    .map(|directory| SaveTarget::Fallback(directory.join(default_name)))
                    .ok_or_else(|| format!("нет диалога сохранения и папки «Загрузки»: {error}"))
            }
        }
    }

    /// `Ok(None)` when the user cancelled.
    fn portal_save_file(default_name: &str) -> Result<Option<PathBuf>, String> {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| run_portal(&context, default_name))
            .map_err(|error| error.to_string())?
    }

    fn run_portal(
        context: &glib::MainContext,
        default_name: &str,
    ) -> Result<Option<PathBuf>, String> {
        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .map_err(|error| error.to_string())?;
        let sender = connection
            .unique_name()
            .ok_or("no unique D-Bus name")?
            .trim_start_matches(':')
            .replace('.', "_");
        let token = format!("multicore_logs_{}", std::process::id());
        let request_path = format!("{PORTAL_PATH}/request/{sender}/{token}");

        let main_loop = glib::MainLoop::new(Some(context), false);
        type PortalAnswer = Rc<RefCell<Option<Result<Option<PathBuf>, String>>>>;
        let answer: PortalAnswer = Rc::default();
        // Subscribed before the call, so an instant answer is not missed.
        let subscription = {
            let answer = answer.clone();
            let main_loop = main_loop.clone();
            connection.signal_subscribe(
                Some(PORTAL),
                Some("org.freedesktop.portal.Request"),
                Some("Response"),
                Some(&request_path),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, _, parameters| {
                    *answer.borrow_mut() = Some(parse_response(parameters));
                    main_loop.quit();
                },
            )
        };

        let options = glib::VariantDict::new(None);
        options.insert_value("handle_token", &glib::ToVariant::to_variant(&token));
        options.insert_value("current_name", &glib::ToVariant::to_variant(default_name));
        options.insert_value("modal", &glib::ToVariant::to_variant(&true));
        let parameters = glib::Variant::tuple_from_iter([
            glib::ToVariant::to_variant(""),
            glib::ToVariant::to_variant("Сохранить лог MULTICORE"),
            options.end(),
        ]);
        let call = connection.call_sync(
            Some(PORTAL),
            PORTAL_PATH,
            "org.freedesktop.portal.FileChooser",
            "SaveFile",
            Some(&parameters),
            Some(glib::VariantTy::new("(o)").expect("static variant type")),
            gio::DBusCallFlags::NONE,
            10_000,
            None::<&gio::Cancellable>,
        );
        if let Err(error) = call {
            connection.signal_unsubscribe(subscription);
            return Err(error.to_string());
        }
        if answer.borrow().is_none() {
            let timeout_loop = main_loop.clone();
            let timeout = glib::timeout_source_new(
                ANSWER_TIMEOUT,
                None,
                glib::Priority::DEFAULT,
                move || {
                    timeout_loop.quit();
                    glib::ControlFlow::Break
                },
            );
            timeout.attach(Some(context));
            main_loop.run();
            timeout.destroy();
        }
        connection.signal_unsubscribe(subscription);
        let answer = answer.borrow_mut().take();
        answer.unwrap_or_else(|| Err("the file chooser did not answer".into()))
    }

    /// `Response (u response, a{sv} results)`: 0 = chosen, 1 = cancelled, 2 = failed.
    fn parse_response(parameters: &glib::Variant) -> Result<Option<PathBuf>, String> {
        let code = parameters.child_value(0).get::<u32>().unwrap_or(2);
        match code {
            0 => {
                let results = glib::VariantDict::new(Some(&parameters.child_value(1)));
                let uri = results
                    .lookup_value("uris", None)
                    .and_then(|uris| uris.get::<Vec<String>>())
                    .and_then(|uris| uris.into_iter().next())
                    .ok_or("the file chooser returned no file")?;
                gio::File::for_uri(&uri)
                    .path()
                    .map(Some)
                    .ok_or_else(|| "the chosen file is not local".to_owned())
            }
            1 => Ok(None),
            _ => Err("the file chooser failed".into()),
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::{OwnerWindow, SaveTarget, fallback_directory};

    pub(super) fn choose(_owner: OwnerWindow, default_name: &str) -> Result<SaveTarget, String> {
        fallback_directory()
            .map(|directory| SaveTarget::Fallback(directory.join(default_name)))
            .ok_or_else(|| "нет папки для сохранения".into())
    }
}

#[cfg(test)]
mod tests {
    use super::xdg_download_dir;
    use std::path::Path;

    #[test]
    fn the_localised_downloads_folder_is_read_from_user_dirs() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("user-dirs.dirs"),
            "# comment\nXDG_DOWNLOAD_DIR=\"$HOME/Загрузки\"\n",
        )
        .unwrap();
        // Only the file under `home` is read; XDG_CONFIG_HOME may point elsewhere on the host.
        if std::env::var_os("XDG_CONFIG_HOME").is_none() {
            assert_eq!(
                xdg_download_dir(home.path()),
                Some(home.path().join("Загрузки"))
            );
        }
        assert_eq!(xdg_download_dir(Path::new("/nonexistent-home")), None);
    }
}
