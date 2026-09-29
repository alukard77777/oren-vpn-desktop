//! Colour emoji drawn as images instead of font glyphs.
//!
//! Slint's software renderer (the fallback on machines without OpenGL 2.0,
//! see `renderer_fallback`) rasterizes glyph outlines only, so colour emoji
//! fonts come out blank or as grey silhouettes. Node and group names come
//! from subscriptions and may start with any emoji, so the whole Noto Emoji
//! 3D set (Apache 2.0 / OFL 1.1, see assets/emoji/LICENSE) is embedded,
//! pre-scaled to 40px WebP, and drawn as an image in both renderers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Layout: b"MCEMOJW1", u32 count, then per entry u16 key length, the key
/// (the emoji sequence as UTF-8 with U+FE0F removed), u32 image length and
/// the WebP image; all integers little-endian.
static PACK: &[u8] = include_bytes!("../assets/emoji/noto-3d-40.pack");
const MAGIC: &[u8] = b"MCEMOJW1";
/// Longer than any sequence in the set (ZWJ families with skin tones).
const MAX_SEQUENCE_CHARS: usize = 16;
const VARIATION_SELECTOR_16: char = '\u{fe0f}';

fn index() -> &'static HashMap<&'static str, &'static [u8]> {
    static INDEX: OnceLock<HashMap<&'static str, &'static [u8]>> = OnceLock::new();
    INDEX.get_or_init(|| parse_pack(PACK).unwrap_or_default())
}

fn parse_pack(pack: &'static [u8]) -> Option<HashMap<&'static str, &'static [u8]>> {
    let mut rest = pack.strip_prefix(MAGIC)?;
    let mut take = |length: usize| -> Option<&'static [u8]> {
        let (head, tail) = rest.split_at_checked(length)?;
        rest = tail;
        Some(head)
    };
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let mut entries = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let key_length = u16::from_le_bytes(take(2)?.try_into().ok()?);
        let key = std::str::from_utf8(take(key_length.into())?).ok()?;
        let image_length = u32::from_le_bytes(take(4)?.try_into().ok()?);
        let image = take(image_length as usize)?;
        // Keycap bases and similar plain ASCII would turn "1 Server" into an icon.
        if !key.is_ascii() {
            entries.insert(key, image);
        }
    }
    Some(entries)
}

/// Byte length of the longest emoji sequence at the start of `text`
/// (including a trailing U+FE0F), if it starts with one.
pub(crate) fn leading_emoji_len(text: &str) -> Option<usize> {
    let index = index();
    let mut key = String::new();
    let mut longest = None;
    for (offset, character) in text.char_indices().take(MAX_SEQUENCE_CHARS) {
        if character != VARIATION_SELECTOR_16 {
            key.push(character);
        }
        if index.contains_key(key.as_str()) {
            longest = Some(offset + character.len_utf8());
        }
    }
    longest
}

/// The image for an emoji sequence, or an empty image (width 0) when it is
/// not an emoji in the set; the UI then falls back to drawing it as text.
pub(crate) fn image_for(emoji: &str) -> Image {
    thread_local! {
        static DECODED: RefCell<HashMap<String, Image>> = RefCell::new(HashMap::new());
    }
    if emoji.is_empty() {
        return Image::default();
    }
    let key = emoji.replace(VARIATION_SELECTOR_16, "");
    DECODED.with(|decoded| {
        if let Some(image) = decoded.borrow().get(&key) {
            return image.clone();
        }
        let image = index()
            .get(key.as_str())
            .and_then(|bytes| decode(bytes))
            .unwrap_or_default();
        decoded.borrow_mut().insert(key, image.clone());
        image
    })
}

fn decode(bytes: &[u8]) -> Option<Image> {
    let rgba = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
        .ok()?
        .to_rgba8();
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    Some(Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_set_covers_flags_symbols_and_sequences() {
        assert!(index().len() > 3_900, "{}", index().len());
        for emoji in ["🇺🇸", "🇸🇪", "⚙️", "⚙", "🎮", "🌍", "👨‍👩‍👧", "👍🏽", "🏳️‍🌈"]
        {
            let image = image_for(emoji);
            assert_eq!(image.size().width, 40, "{emoji}");
        }
        assert_eq!(image_for("↻").size().width, 0);
        assert_eq!(image_for("").size().width, 0);
    }

    #[test]
    fn the_longest_leading_sequence_wins_and_plain_text_does_not_match() {
        assert_eq!(leading_emoji_len("🇩🇪 Frankfurt"), Some("🇩🇪".len()));
        assert_eq!(leading_emoji_len("⚙️ Auto"), Some("⚙️".len()));
        assert_eq!(leading_emoji_len("👨‍👩‍👧 Family"), Some("👨‍👩‍👧".len()));
        assert_eq!(leading_emoji_len("🦄Fast"), Some("🦄".len()));
        assert_eq!(leading_emoji_len("1 Server"), None);
        assert_eq!(leading_emoji_len("Frankfurt 🇩🇪"), None);
        assert_eq!(leading_emoji_len(""), None);
    }
}
