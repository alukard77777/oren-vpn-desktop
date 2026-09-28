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

pub(crate) fn read_subscription_text() -> Result<String, ClipboardTextError> {
    let mut clipboard = ClipboardContext::new().map_err(|_| ClipboardTextError::Unavailable)?;
    let contents = clipboard
        .get_contents()
        .map_err(|_| ClipboardTextError::Unavailable)?;
    normalize_subscription_text(&contents)
}

/// Used by the "copy diagnostics" action: the caller is responsible for
/// only ever passing already-redacted text (see
/// `DesktopViewModel::diagnostics_export_text`).
pub(crate) fn write_clipboard_text(text: String) -> Result<(), ClipboardTextError> {
    let mut clipboard = ClipboardContext::new().map_err(|_| ClipboardTextError::Unavailable)?;
    clipboard
        .set_contents(text)
        .map_err(|_| ClipboardTextError::Unavailable)
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
