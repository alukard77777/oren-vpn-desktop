//! The messages the daemon reports to the desktop, by stable code.
//!
//! The daemon sends a [`MessageKey`](crate::MessageKey) (code and arguments) next to the Russian
//! text it always sent; the desktop shows [`MessageKey::localize`](crate::MessageKey::localize)
//! and falls back to the Russian text only for a code it does not know. The Russian template is
//! the catalog `msgid`, so it is written once, here, and translated in the `.po` like every
//! other string. Codes never change meaning once released; a changed message gets a new code.

/// `(code, Russian template)`. Arguments fill `{0}`, `{1}`, … (see [`crate::format`]).
pub const ALL: &[(&str, &str)] = &[];

/// The Russian template of a code.
#[must_use]
pub fn template(code: &str) -> Option<&'static str> {
    ALL.iter()
        .find(|(candidate, _)| *candidate == code)
        .map(|(_, template)| *template)
}
