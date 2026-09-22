pub(crate) const TRAY_ICON_SIZE: u32 = 32;

const TRAY_ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/app-icon-32.rgba"));
#[cfg(any(target_os = "linux", test))]
const STATUS_ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/app-icon-64.rgba"));

pub(crate) fn tray_rgba() -> Vec<u8> {
    TRAY_ICON_RGBA.to_vec()
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn rgba(size: u32) -> Vec<u8> {
    match size {
        32 => TRAY_ICON_RGBA.to_vec(),
        64 => STATUS_ICON_RGBA.to_vec(),
        _ => panic!("unsupported internal app icon size: {size}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{TRAY_ICON_SIZE, rgba, tray_rgba};

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
}
