const SCALE_OVERRIDE: &str = "WINIT_X11_SCALE_FACTOR";

pub(crate) fn resources_set_xft_dpi(resources: &str) -> bool {
    resources.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim() == "Xft.dpi" && value.trim().parse::<f64>().is_ok_and(|dpi| dpi > 0.0)
        })
    })
}

pub(crate) fn needs_unit_scale(
    x11_session: bool,
    scale_override: bool,
    resources: Option<&str>,
) -> bool {
    x11_session && !scale_override && !resources.is_some_and(resources_set_xft_dpi)
}

fn resource_manager() -> Option<String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let reply = connection
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            u32::MAX / 4,
        )
        .ok()?
        .reply()
        .ok()?;
    String::from_utf8(reply.value).ok()
}

pub(crate) fn prefer_desktop_scale() {
    let x11_session = std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty())
        && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|display| display.is_empty());
    let scale_override = std::env::var_os(SCALE_OVERRIDE).is_some();
    if !x11_session || scale_override {
        return;
    }
    if needs_unit_scale(x11_session, scale_override, resource_manager().as_deref()) {
        unsafe { std::env::set_var(SCALE_OVERRIDE, "1") };
    }
}

#[cfg(test)]
mod tests {
    use super::{needs_unit_scale, resources_set_xft_dpi};

    #[test]
    fn xft_dpi_is_detected_only_when_it_is_a_positive_number() {
        assert!(resources_set_xft_dpi("Xcursor.size:\t24\nXft.dpi:\t144\n"));
        assert!(resources_set_xft_dpi("Xft.dpi: 96.5"));
        assert!(!resources_set_xft_dpi("Xcursor.size:\t24\n"));
        assert!(!resources_set_xft_dpi("Xft.dpi:\tlarge\n"));
        assert!(!resources_set_xft_dpi("Xft.dpi:\t0\n"));
        assert!(!resources_set_xft_dpi("Xft.dpiX:\t144\n"));
    }

    #[test]
    fn unit_scale_applies_only_to_x11_without_dpi_or_override() {
        assert!(needs_unit_scale(true, false, None));
        assert!(needs_unit_scale(true, false, Some("Xcursor.size: 24")));
        assert!(!needs_unit_scale(true, false, Some("Xft.dpi: 144")));
        assert!(!needs_unit_scale(true, true, None));
        assert!(!needs_unit_scale(false, false, None));
    }
}
