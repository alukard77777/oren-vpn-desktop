//! Falls back to Slint's software renderer where OpenGL 2.0 is unavailable.
//!
//! FemtoVG needs OpenGL 2.0+. A Windows machine without a vendor display
//! driver (a fresh install, a Hyper-V/VirtualBox guest with the basic display
//! adapter, some RDP sessions) only has the built-in OpenGL 1.1, and the app
//! used to exit before showing any window ("Failed to initialize OpenGL
//! driver: Could not locate glCreateShader symbol"). Slint's own renderer
//! fallback only covers a renderer that cannot be constructed, not a GL
//! context that fails when the window is first shown, and the platform can
//! only be selected once per process - so the app records the failure and
//! restarts itself with the software renderer.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::{fs, io};

/// Presence of this file under the user data root selects the software
/// renderer on every later launch, so the failed GPU attempt isn't repeated.
const MARKER_FILE: &str = "software-renderer";
/// Set on the restarted process (and usable manually) to select the software
/// renderer even if the marker could not be written.
const SOFTWARE_RENDERER_ENV: &str = "MULTICORE_SOFTWARE_RENDERER";

pub fn software_renderer_requested() -> bool {
    std::env::var_os(SOFTWARE_RENDERER_ENV).is_some()
        || marker_path().is_some_and(|path| path.is_file())
}

pub fn select_software_renderer() -> Result<(), slint::PlatformError> {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .select()
}

/// Whether a platform error from creating or showing a window means the GPU
/// renderer itself could not start, as opposed to any other startup failure.
pub fn is_gpu_renderer_failure(error: &slint::PlatformError) -> bool {
    is_gpu_renderer_failure_text(&error.to_string())
}

fn is_gpu_renderer_failure_text(text: &str) -> bool {
    [
        "OpenGL",
        "glutin",
        "EGL",
        "WGL",
        "GL context",
        "suitable renderer",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Records the fallback and runs a new instance with the same arguments,
/// waiting for it to exit. The caller must already have released the
/// single-instance guard. Waiting matters when the updater launched this
/// process: it rolls the update back if the process it started exits before
/// the new version confirms its start, which the restarted instance does.
pub fn remember_and_relaunch(arguments: Vec<OsString>) -> io::Result<()> {
    if let Some(path) = marker_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(error) = fs::write(&path, b"OpenGL 2.0 renderer failed to start\n") {
            eprintln!("software renderer preference not saved: {error}");
        }
    }
    Command::new(std::env::current_exe()?)
        .args(arguments)
        .env(SOFTWARE_RENDERER_ENV, "1")
        .spawn()?
        .wait()
        .map(drop)
}

fn marker_path() -> Option<PathBuf> {
    multicore_platform::dirs::user_data_root()
        .ok()
        .map(|root| root.join(MARKER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_renderer_start_failures_trigger_the_fallback() {
        assert!(is_gpu_renderer_failure_text(
            "Failed to initialize OpenGL driver: Could not locate glCreateShader symbol"
        ));
        assert!(is_gpu_renderer_failure_text(
            "Winit backend failed to find a suitable renderer: no display"
        ));
        assert!(!is_gpu_renderer_failure_text(
            "No default Slint platform was selected"
        ));
        assert!(!is_gpu_renderer_failure_text("event loop already running"));
    }
}
