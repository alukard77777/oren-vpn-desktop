//! Small logos and group icons drawn pixel-exact.
//!
//! Slint scales an `Image` source to its element with a plain bilinear sample and no mipmaps,
//! so a 256 px provider logo drawn in a 12 px slot aliases into a blurry, "compressed" blob,
//! worse at fractional scale factors. Instead every small image is resampled once on the CPU
//! with a Lanczos filter to the exact physical pixel box it is drawn in (logical size times the
//! window scale factor) and cached per size, so the renderer draws it 1:1.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use image::{RgbaImage, imageops};

/// Physical pixels of a `logical` px square at `scale`.
pub(crate) fn physical_px(logical: f32, scale: f32) -> u32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (logical * scale).round().clamp(1.0, 1024.0) as u32
}

/// `source` fitted into a `box_px` square (aspect kept, never cropped) with a high-quality
/// filter: Lanczos3 when shrinking, Catmull-Rom when a small source has to grow.
pub(crate) fn fit(source: &RgbaImage, box_px: u32) -> RgbaImage {
    let (width, height) = source.dimensions();
    if width == 0 || height == 0 {
        return RgbaImage::new(1, 1);
    }
    let scale = (f64::from(box_px) / f64::from(width)).min(f64::from(box_px) / f64::from(height));
    let target_width = (f64::from(width) * scale)
        .round()
        .clamp(1.0, f64::from(box_px)) as u32;
    let target_height = (f64::from(height) * scale)
        .round()
        .clamp(1.0, f64::from(box_px)) as u32;
    if (target_width, target_height) == (width, height) {
        return source.clone();
    }
    let filter = if target_width < width {
        imageops::FilterType::Lanczos3
    } else {
        imageops::FilterType::CatmullRom
    };
    imageops::resize(source, target_width, target_height, filter)
}

pub(crate) fn to_slint(image: &RgbaImage) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        image.as_raw(),
        image.width(),
        image.height(),
    );
    slint::Image::from_rgba8(buffer)
}

/// A decoded source plus its resampled sizes. Lives on the UI thread (slint images are not
/// `Send`); the source itself may be decoded anywhere.
#[derive(Clone)]
pub(crate) struct ScaledImage {
    source: Arc<RgbaImage>,
    sizes: HashMap<u32, slint::Image>,
}

impl ScaledImage {
    pub(crate) fn new(source: Arc<RgbaImage>) -> Self {
        Self {
            source,
            sizes: HashMap::new(),
        }
    }

    pub(crate) fn at(&mut self, box_px: u32) -> slint::Image {
        self.sizes
            .entry(box_px)
            .or_insert_with(|| to_slint(&fit(&self.source, box_px)))
            .clone()
    }
}

pub(crate) fn decode_png(bytes: &[u8]) -> Option<RgbaImage> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let (width, height) = (image.width(), image.height());
    (width > 0 && height > 0 && width <= 1024 && height <= 1024).then(|| image.into_rgba8())
}

/// Group icons by the daemon's opaque content id, decoded off the UI thread.
const MAX_GROUP_ICONS: usize = 128;

fn group_icon_sources() -> &'static Mutex<HashMap<String, Arc<RgbaImage>>> {
    static SOURCES: OnceLock<Mutex<HashMap<String, Arc<RgbaImage>>>> = OnceLock::new();
    SOURCES.get_or_init(Mutex::default)
}

pub(crate) fn has_group_icon(id: &str) -> bool {
    group_icon_sources()
        .lock()
        .expect("group icon lock")
        .contains_key(id)
}

/// Decodes and keeps a group icon; `false` when the bytes are not a usable PNG.
pub(crate) fn store_group_icon(id: &str, png: &[u8]) -> bool {
    let Some(image) = decode_png(png) else {
        return false;
    };
    let mut sources = group_icon_sources().lock().expect("group icon lock");
    if sources.len() >= MAX_GROUP_ICONS && !sources.contains_key(id) {
        sources.clear();
    }
    sources.insert(id.to_owned(), Arc::new(image));
    true
}

thread_local! {
    static GROUP_ICON_IMAGES: std::cell::RefCell<HashMap<String, ScaledImage>> =
        std::cell::RefCell::new(HashMap::new());
}

/// The group icon `id` fitted into a `logical` px square at `scale`; `None` until the icon
/// has been fetched (callers fall back to the emoji or glyph).
pub(crate) fn group_icon(id: &str, logical: f32, scale: f32) -> Option<slint::Image> {
    let box_px = physical_px(logical, scale);
    GROUP_ICON_IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        if let Some(cached) = images.get_mut(id) {
            return Some(cached.at(box_px));
        }
        let source = group_icon_sources()
            .lock()
            .expect("group icon lock")
            .get(id)
            .cloned()?;
        if images.len() >= MAX_GROUP_ICONS {
            images.clear();
        }
        let mut scaled = ScaledImage::new(source);
        let image = scaled.at(box_px);
        images.insert(id.to_owned(), scaled);
        Some(image)
    })
}

#[cfg(test)]
mod tests {
    use super::{ScaledImage, decode_png, fit, physical_px};
    use image::{Rgba, RgbaImage};
    use std::sync::Arc;

    #[test]
    fn physical_size_follows_the_scale_factor() {
        assert_eq!(physical_px(12.0, 1.0), 12);
        assert_eq!(physical_px(12.0, 1.25), 15);
        assert_eq!(physical_px(12.0, 1.5), 18);
        assert_eq!(physical_px(15.0, 1.25), 19);
        assert_eq!(physical_px(22.0, 2.0), 44);
        assert_eq!(physical_px(12.0, f32::NAN), 12);
    }

    #[test]
    fn fit_keeps_aspect_inside_the_box_and_grows_small_sources() {
        let wide = RgbaImage::from_pixel(256, 128, Rgba([255, 0, 0, 255]));
        assert_eq!(fit(&wide, 18).dimensions(), (18, 9));
        let tall = RgbaImage::from_pixel(64, 128, Rgba([255, 0, 0, 255]));
        assert_eq!(fit(&tall, 15).dimensions(), (8, 15));
        let tiny = RgbaImage::from_pixel(8, 8, Rgba([255, 0, 0, 255]));
        assert_eq!(fit(&tiny, 30).dimensions(), (30, 30));
        let exact = RgbaImage::from_pixel(15, 15, Rgba([1, 2, 3, 4]));
        assert_eq!(fit(&exact, 15), exact);
    }

    #[test]
    fn downscaled_edges_stay_sharp_instead_of_smearing() {
        // A hard 50% vertical edge: after a 256 -> 16 shrink the left column stays
        // fully opaque black and the right one fully white (no grey bleed across the box).
        let mut source = RgbaImage::from_pixel(256, 256, Rgba([255, 255, 255, 255]));
        for y in 0..256 {
            for x in 0..128 {
                source.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let small = fit(&source, 16);
        assert!(small.get_pixel(0, 8)[0] < 8);
        assert!(small.get_pixel(15, 8)[0] > 247);
    }

    #[test]
    fn scaled_sizes_are_cached_per_box() {
        let mut scaled = ScaledImage::new(Arc::new(RgbaImage::from_pixel(
            64,
            64,
            Rgba([9, 9, 9, 255]),
        )));
        let first = scaled.at(15);
        assert_eq!((first.size().width, first.size().height), (15, 15));
        assert_eq!(scaled.at(18).size().width, 18);
        assert_eq!(scaled.sizes.len(), 2);
    }

    #[test]
    fn only_bounded_pngs_decode() {
        assert!(decode_png(b"not a png").is_none());
        let mut bytes = std::io::Cursor::new(Vec::new());
        RgbaImage::from_pixel(3, 2, Rgba([1, 2, 3, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        assert_eq!(decode_png(bytes.get_ref()).unwrap().dimensions(), (3, 2));
    }
}
