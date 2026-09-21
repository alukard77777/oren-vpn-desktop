use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::path::Path;

const MAX_LOGO_BYTES: u64 = 512 * 1024;
const MAX_CACHED_LOGOS: usize = 32;

thread_local! {
    static DECODED: RefCell<HashMap<u64, Option<slint::Image>>> = RefCell::new(HashMap::new());
}

pub(crate) fn load(path: &Path) -> Option<slint::Image> {
    let bytes = read_bounded(path)?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let key = hasher.finish();
    DECODED.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(image) = cache.get(&key) {
            return image.clone();
        }
        if cache.len() >= MAX_CACHED_LOGOS {
            cache.clear();
        }
        let image = decode_png(&bytes);
        cache.insert(key, image.clone());
        image
    })
}

fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_LOGO_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (!bytes.is_empty() && bytes.len() as u64 <= MAX_LOGO_BYTES).then_some(bytes)
}

fn decode_png(bytes: &[u8]) -> Option<slint::Image> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .ok()?
        .into_rgba8();
    let (width, height) = image.dimensions();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        image.as_raw(),
        width,
        height,
    );
    Some(slint::Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::{MAX_LOGO_BYTES, load};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, image::Rgba([12, 34, 56, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn png_without_an_extension_is_decoded_by_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("13");
        std::fs::write(&path, png(3, 2)).unwrap();
        let image = load(&path).unwrap();
        assert_eq!((image.size().width, image.size().height), (3, 2));
    }

    #[test]
    fn replaced_content_at_the_same_path_is_not_served_from_cache() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("16");
        std::fs::write(&path, png(4, 4)).unwrap();
        assert_eq!(load(&path).unwrap().size().width, 4);
        std::fs::write(&path, png(6, 5)).unwrap();
        assert_eq!(load(&path).unwrap().size().width, 6);
    }

    #[test]
    fn missing_empty_oversized_and_non_png_files_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load(&temp.path().join("missing")).is_none());
        let empty = temp.path().join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert!(load(&empty).is_none());
        let text = temp.path().join("text");
        std::fs::write(&text, b"<svg/>").unwrap();
        assert!(load(&text).is_none());
        let oversized = temp.path().join("oversized");
        let mut bytes = png(2, 2);
        bytes.resize(MAX_LOGO_BYTES as usize + 1, 0);
        std::fs::write(&oversized, bytes).unwrap();
        assert!(load(&oversized).is_none());
    }
}
