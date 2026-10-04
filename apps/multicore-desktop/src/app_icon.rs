#[cfg(any(windows, test))]
pub(crate) const TRAY_ICON_SIZE: u32 = 32;

const TRAY_ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/app-icon-32.rgba"));
const STATUS_ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/app-icon-64.rgba"));

#[cfg(test)]
pub(crate) fn tray_rgba() -> Vec<u8> {
    TRAY_ICON_RGBA.to_vec()
}

#[cfg(test)]
pub(crate) fn rgba(size: u32) -> Vec<u8> {
    match size {
        32 => TRAY_ICON_RGBA.to_vec(),
        64 => STATUS_ICON_RGBA.to_vec(),
        _ => panic!("unsupported internal app icon size: {size}"),
    }
}

/// What the tray icon shows; each badge differs in shape, not only in colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TrayIconState {
    #[default]
    Disconnected,
    Connecting,
    /// Connected through the system proxy.
    Proxy,
    /// Connected through TUN (all traffic).
    Tun,
    Error,
}

impl TrayIconState {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::Disconnected,
        Self::Connecting,
        Self::Proxy,
        Self::Tun,
        Self::Error,
    ];
}

macro_rules! state_icon {
    ($state:literal, $size:literal) => {
        include_bytes!(concat!(
            env!("OUT_DIR"),
            "/app-icon-",
            $state,
            "-",
            $size,
            ".rgba"
        ))
    };
}

/// The tray icon for `state`, `size` px square (32 or 64), as straight RGBA.
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) fn tray_state_rgba(state: TrayIconState, size: u32) -> Vec<u8> {
    let pixels: &[u8] = match (state, size) {
        (TrayIconState::Disconnected, 32) => TRAY_ICON_RGBA,
        (TrayIconState::Disconnected, 64) => STATUS_ICON_RGBA,
        (TrayIconState::Connecting, 32) => state_icon!("connecting", "32"),
        (TrayIconState::Connecting, 64) => state_icon!("connecting", "64"),
        (TrayIconState::Proxy, 32) => state_icon!("proxy", "32"),
        (TrayIconState::Proxy, 64) => state_icon!("proxy", "64"),
        (TrayIconState::Tun, 32) => state_icon!("tun", "32"),
        (TrayIconState::Tun, 64) => state_icon!("tun", "64"),
        (TrayIconState::Error, 32) => state_icon!("error", "32"),
        (TrayIconState::Error, 64) => state_icon!("error", "64"),
        _ => panic!("unsupported internal tray icon size: {size}"),
    };
    pixels.to_vec()
}

#[cfg(test)]
mod tests {
    use super::{TRAY_ICON_SIZE, TrayIconState, rgba, tray_rgba, tray_state_rgba};

    #[test]
    fn tray_and_executable_icons_are_generated_from_the_canonical_svg() {
        let runtime_source = include_str!("app_icon.rs");
        let build_source = include_str!("../build.rs");
        let removed_geometry_helper = ["fn ring_", "coverage"].concat();

        assert!(runtime_source.contains("app-icon-32.rgba"));
        assert!(!runtime_source.contains(&removed_geometry_helper));
        assert!(build_source.contains("include_bytes!(\"assets/app-icon.svg\")"));
        assert!(build_source.contains("app-icon-{size}.rgba"));
        assert!(build_source.contains("TRAY_ICON_SIZES: [u32; 2] = [32, 64]"));
    }

    #[test]
    fn icon_is_rgba_with_transparency_and_both_brand_colors() {
        let pixels = tray_rgba();
        assert_eq!(pixels.len(), (TRAY_ICON_SIZE * TRAY_ICON_SIZE * 4) as usize);

        let mut has_transparency = false;
        let mut has_blue = false;
        let mut has_neutral = false;
        for pixel in pixels.as_chunks::<4>().0 {
            has_transparency |= pixel[3] == 0;
            has_blue |= pixel[2] > 180 && pixel[0] < 120 && pixel[3] > 200;
            has_neutral |= pixel[0] > 190 && pixel[1] > 190 && pixel[2] > 190 && pixel[3] > 200;
        }

        assert!(has_transparency);
        assert!(has_blue);
        assert!(has_neutral);
    }

    #[test]
    fn linux_tray_sizes_are_generated_from_the_same_canonical_icon() {
        assert_eq!(rgba(32), tray_rgba());
        assert_eq!(rgba(64).len(), 64 * 64 * 4);
    }

    /// Opaque-enough pixels: what a monochrome (or colour-blind) look still tells apart.
    fn silhouette(pixels: &[u8]) -> Vec<bool> {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[3] > 127)
            .collect()
    }

    #[test]
    fn every_tray_state_has_its_own_silhouette_not_only_its_own_colour() {
        for size in [32, 64] {
            let silhouettes: Vec<(TrayIconState, Vec<bool>)> = TrayIconState::ALL
                .iter()
                .map(|state| (*state, silhouette(&tray_state_rgba(*state, size))))
                .collect();
            for (index, (state, shape)) in silhouettes.iter().enumerate() {
                for (other, other_shape) in &silhouettes[index + 1..] {
                    let differing = shape
                        .iter()
                        .zip(other_shape)
                        .filter(|(left, right)| left != right)
                        .count();
                    assert!(
                        differing * 100 >= (size * size) as usize,
                        "{state:?} and {other:?} differ in only {differing} pixels at {size} px"
                    );
                }
            }
        }
    }

    #[test]
    fn disconnected_is_the_plain_logo_and_badges_keep_most_of_it() {
        assert_eq!(
            tray_state_rgba(TrayIconState::Disconnected, 32),
            tray_rgba()
        );
        assert_eq!(tray_state_rgba(TrayIconState::Disconnected, 64), rgba(64));
        let logo = silhouette(&tray_rgba());
        let logo_pixels = logo.iter().filter(|opaque| **opaque).count();
        for state in TrayIconState::ALL {
            let badged = silhouette(&tray_state_rgba(state, 32));
            let kept = logo
                .iter()
                .zip(&badged)
                .filter(|(logo, badged)| **logo && **badged)
                .count();
            assert!(kept * 10 >= logo_pixels * 9, "{state:?} hides the logo");
        }
    }
}
