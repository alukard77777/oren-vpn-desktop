//! MULTICORE's interface languages: Russian (the source language and the default) and English.
//!
//! One gettext catalog, `apps/multicore-desktop/translations/en/LC_MESSAGES/multicore-desktop.po`,
//! serves both halves of the desktop app: Slint bundles it for every `@tr("…")` in the `.slint`
//! files, and `build.rs` compiles the same file into the table [`tr!`] reads for strings built
//! in Rust. A `msgid` is the exact Russian text, so Russian output is byte-identical to the
//! source text and needs no catalog at all.
//!
//! Templates use Slint's placeholder syntax so one entry works on both sides: `{}` takes the
//! next argument, `{0}`/`{1}` a positional one, `{n}` the count of a plural entry, and `{{`/`}}`
//! are literal braces.
//!
//! The daemon reports what it says through [`MessageKey`]s (a stable code plus arguments) next
//! to its Russian text; [`daemon_messages`] holds the Russian templates of those codes, which
//! the desktop translates like any other catalog entry.

use std::cell::Cell;
use std::fmt::{self, Display, Write as _};
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

pub mod daemon_messages;

mod catalog {
    include!(concat!(env!("OUT_DIR"), "/catalog.rs"));
}

/// An interface language.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    /// The source language of every string, and the default.
    #[default]
    Ru,
    En,
}

impl Lang {
    pub const ALL: [Self; 2] = [Self::Ru, Self::En];

    /// The two-letter code stored in preferences and sent by the daemon.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Ru => "ru",
            Self::En => "en",
        }
    }

    /// Parses a language tag the way the provider header `app-language` is read: case-insensitive,
    /// the base language of a regional tag (`ru-RU`, `en_US`), surrounding spaces ignored.
    /// Anything but Russian or English is `None`.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        let tag = tag.trim();
        let base = tag.split(['-', '_']).next().unwrap_or_default();
        if base.eq_ignore_ascii_case("ru") {
            Some(Self::Ru)
        } else if base.eq_ignore_ascii_case("en") {
            Some(Self::En)
        } else {
            None
        }
    }

    /// The bundled Slint translation of this language: the empty string selects the source
    /// (Russian) strings, which `slint::select_bundled_translation` treats as the original.
    #[must_use]
    pub const fn slint_bundle(self) -> &'static str {
        match self {
            Self::Ru => "",
            Self::En => "en",
        }
    }
}

impl Display for Lang {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

thread_local! {
    /// A per-thread language that wins over the process-wide one (see [`ThreadLanguage`]).
    static THREAD_OVERRIDE: Cell<Option<Lang>> = const { Cell::new(None) };
}

fn encode(lang: Lang) -> u8 {
    match lang {
        Lang::Ru => 0,
        Lang::En => 1,
    }
}

/// The language strings are produced in right now.
#[must_use]
pub fn current() -> Lang {
    if let Some(lang) = THREAD_OVERRIDE.with(Cell::get) {
        return lang;
    }
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Ru,
    }
}

/// Switches the process-wide language. Strings already produced keep their language.
pub fn set_current(lang: Lang) {
    CURRENT.store(encode(lang), Ordering::Relaxed);
}

/// Makes this thread produce strings in `lang` until the guard drops, whatever the
/// process-wide language is. Tests use it so parallel tests never see each other's language.
#[must_use = "the thread language reverts when the guard drops"]
pub struct ThreadLanguage {
    previous: Option<Lang>,
}

impl ThreadLanguage {
    pub fn set(lang: Lang) -> Self {
        Self {
            previous: THREAD_OVERRIDE.with(|cell| cell.replace(Some(lang))),
        }
    }
}

impl Drop for ThreadLanguage {
    fn drop(&mut self) {
        THREAD_OVERRIDE.with(|cell| cell.set(self.previous));
    }
}

/// The text of `msgid` (the Russian source text) in the current language.
#[must_use]
pub fn translate(msgid: &'static str) -> &'static str {
    translate_in(current(), msgid)
}

/// The text of `msgid` in `lang`. A string the catalog lacks stays Russian (the catalog
/// completeness tests keep that from happening).
#[must_use]
pub fn translate_in(lang: Lang, msgid: &'static str) -> &'static str {
    match lang {
        Lang::Ru => msgid,
        Lang::En => english(msgid).unwrap_or(msgid),
    }
}

/// The English text of a Russian `msgid`, when the catalog has one.
#[must_use]
pub fn english(msgid: &str) -> Option<&'static str> {
    catalog::SINGULAR
        .binary_search_by(|(candidate, _)| (*candidate).cmp(msgid))
        .ok()
        .map(|index| catalog::SINGULAR[index].1)
}

/// The template of a plural entry for `n` in the current language. Russian keeps the source
/// forms with the source language's two-form rule (`msgid` for one, `msgid_plural` otherwise),
/// exactly as Slint does for `@tr("…" | "…" % n)`.
#[must_use]
pub fn translate_plural(msgid: &'static str, msgid_plural: &'static str, n: i64) -> &'static str {
    let one = n == 1;
    match current() {
        Lang::Ru => {
            if one {
                msgid
            } else {
                msgid_plural
            }
        }
        Lang::En => english_plural(msgid, msgid_plural)
            .map(|(singular, plural)| if one { singular } else { plural })
            .unwrap_or(if one { msgid } else { msgid_plural }),
    }
}

/// The English singular and plural forms of a plural entry, when the catalog has them.
#[must_use]
pub fn english_plural(msgid: &str, msgid_plural: &str) -> Option<(&'static str, &'static str)> {
    catalog::PLURAL
        .binary_search_by(|((candidate, candidate_plural), _)| {
            (*candidate, *candidate_plural).cmp(&(msgid, msgid_plural))
        })
        .ok()
        .map(|index| catalog::PLURAL[index].1)
}

/// Every `(msgid, English)` pair of the catalog (catalog tests).
pub fn catalog_entries() -> impl Iterator<Item = (&'static str, &'static str)> {
    catalog::SINGULAR.iter().copied()
}

/// Every plural entry: `((msgid, msgid_plural), (English one, English other))`.
pub fn catalog_plural_entries()
-> impl Iterator<Item = ((&'static str, &'static str), (&'static str, &'static str))> {
    catalog::PLURAL.iter().copied()
}

/// Fills a template the way Slint's `@tr()` does: `{}` is the next argument, `{0}` a positional
/// one, `{n}` the plural count (`n`), `{{`/`}}` literal braces. An unknown placeholder is kept.
#[must_use]
pub fn format(template: &str, args: &[&dyn Display]) -> String {
    format_with_count(template, args, None)
}

/// [`format`] with the `{n}` count of a plural entry.
#[must_use]
pub fn format_with_count(template: &str, args: &[&dyn Display], n: Option<i64>) -> String {
    let mut output = String::with_capacity(template.len() + 16);
    let mut next = 0_usize;
    let mut rest = template;
    while let Some(position) = rest.find(['{', '}']) {
        output.push_str(&rest[..position]);
        let tail = &rest[position..];
        if tail.starts_with("{{") {
            output.push('{');
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with("}}") {
            output.push('}');
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('}') {
            output.push('}');
            rest = &tail[1..];
            continue;
        }
        let Some(end) = tail.find('}') else {
            output.push_str(tail);
            rest = "";
            break;
        };
        let name = tail[1..end].trim();
        let value = if name.is_empty() {
            next += 1;
            args.get(next - 1).copied()
        } else if let Ok(index) = name.parse::<usize>() {
            args.get(index).copied()
        } else {
            None
        };
        match (value, name, n) {
            (Some(value), _, _) => {
                let _ = write!(output, "{value}");
            }
            (None, "n", Some(count)) => {
                let _ = write!(output, "{count}");
            }
            _ => output.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    output
}

/// A stable message the daemon reports (in a status, an event or a troubleshooting card): a
/// code from [`daemon_messages`] and the template's arguments. The daemon sends its Russian
/// text alongside, for clients that predate the code.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MessageKey {
    pub code: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

impl MessageKey {
    #[must_use]
    pub fn new(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_args(code: &str, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            code: code.to_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The text in the current language, or `None` for a code this build does not know.
    #[must_use]
    pub fn localize(&self) -> Option<String> {
        let template = daemon_messages::template(&self.code)?;
        let args = self
            .args
            .iter()
            .map(|arg| arg as &dyn Display)
            .collect::<Vec<_>>();
        Some(format(translate(template), &args))
    }

    /// The Russian text (what the daemon sends as `message`).
    #[must_use]
    pub fn russian(&self) -> Option<String> {
        let _russian = ThreadLanguage::set(Lang::Ru);
        self.localize()
    }
}

/// The text of a Russian source string in the current language.
///
/// `tr!("Подключить")` is a `&'static str`; with arguments, `tr!("Осталось {} ГБ", left)` fills
/// the translated template (Slint's placeholder syntax, see [`format`]) and is a `String`.
/// The literal is the catalog `msgid`, so keep it a literal: the catalog tests find every
/// `tr!` in the sources.
#[macro_export]
macro_rules! tr {
    ($msgid:literal $(,)?) => {
        $crate::translate($msgid)
    };
    ($msgid:literal, $($arg:expr),+ $(,)?) => {
        $crate::format($crate::translate($msgid), &[$(&$arg as &dyn ::std::fmt::Display),+])
    };
}

/// A plural entry: `trn!("{n} день", "{n} дн.", count)`, optionally with more arguments for
/// `{}`/`{0}` placeholders. `{n}` is the count. Always a `String`.
#[macro_export]
macro_rules! trn {
    ($msgid:literal, $plural:literal, $n:expr $(, $arg:expr)* $(,)?) => {{
        let count: i64 = ::std::convert::TryInto::try_into($n).unwrap_or(i64::MAX);
        $crate::format_with_count(
            $crate::translate_plural($msgid, $plural, count),
            &[$(&$arg as &dyn ::std::fmt::Display),*],
            Some(count),
        )
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_parse_to_their_base_language() {
        for (tag, lang) in [
            ("ru", Some(Lang::Ru)),
            ("RU", Some(Lang::Ru)),
            ("ru-RU", Some(Lang::Ru)),
            ("ru_ru", Some(Lang::Ru)),
            (" en ", Some(Lang::En)),
            ("EN", Some(Lang::En)),
            ("en-US", Some(Lang::En)),
            ("en_GB", Some(Lang::En)),
            ("de", None),
            ("", None),
            ("english", None),
            ("rus", None),
            ("-en", None),
        ] {
            assert_eq!(Lang::from_tag(tag), lang, "{tag:?}");
        }
    }

    #[test]
    fn lang_serializes_as_its_code() {
        assert_eq!(serde_json::to_string(&Lang::En).unwrap(), "\"en\"");
        assert_eq!(serde_json::from_str::<Lang>("\"ru\"").unwrap(), Lang::Ru);
        assert_eq!(Lang::default(), Lang::Ru);
        assert_eq!(Lang::Ru.slint_bundle(), "");
        assert_eq!(Lang::En.slint_bundle(), "en");
    }

    #[test]
    fn format_follows_slint_placeholders() {
        assert_eq!(format("Hello", &[]), "Hello");
        assert_eq!(format("Hello {}!", &[&"world"]), "Hello world!");
        assert_eq!(format("{1} -{0}-", &[&45, &"World"]), "World -45-");
        assert_eq!(format("{{}} {}", &[&1]), "{} 1");
        assert_eq!(format("{missing} {}", &[&1]), "{missing} 1");
        assert_eq!(format_with_count("{n} days", &[], Some(3)), "3 days");
        assert_eq!(format("unclosed {", &[&1]), "unclosed {");
    }

    #[test]
    fn russian_is_the_source_text() {
        let _lang = ThreadLanguage::set(Lang::Ru);
        assert_eq!(tr!("Язык не переведён"), "Язык не переведён");
        assert_eq!(tr!("{} из {}", 1, 2), "1 из 2");
        assert_eq!(trn!("{n} день", "{n} дн.", 1), "1 день");
        assert_eq!(trn!("{n} день", "{n} дн.", 5), "5 дн.");
    }

    #[test]
    fn english_comes_from_the_catalog() {
        let _lang = ThreadLanguage::set(Lang::En);
        let (msgid, english) = catalog_entries().next().expect("the catalog has entries");
        assert_eq!(translate(msgid), english);
        // Unknown strings stay as they are.
        assert_eq!(translate("Строки нет в каталоге"), "Строки нет в каталоге");
    }

    #[test]
    fn thread_language_reverts() {
        let outer = current();
        {
            let _en = ThreadLanguage::set(Lang::En);
            assert_eq!(current(), Lang::En);
            {
                let _ru = ThreadLanguage::set(Lang::Ru);
                assert_eq!(current(), Lang::Ru);
            }
            assert_eq!(current(), Lang::En);
        }
        assert_eq!(current(), outer);
    }

    #[test]
    fn catalog_lookups_are_sorted_for_binary_search() {
        let entries = catalog_entries().collect::<Vec<_>>();
        assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let plurals = catalog_plural_entries().collect::<Vec<_>>();
        assert!(plurals.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for ((msgid, plural), forms) in plurals {
            assert_eq!(english_plural(msgid, plural), Some(forms));
        }
    }

    #[test]
    fn every_daemon_message_has_an_english_text() {
        for (code, template) in daemon_messages::ALL {
            assert!(
                english(template).is_some(),
                "daemon message {code} has no English text: {template:?}"
            );
            assert_eq!(daemon_messages::template(code), Some(*template));
        }
    }

    #[test]
    fn message_keys_localize_through_the_catalog() {
        let Some((code, template)) = daemon_messages::ALL.first() else {
            return;
        };
        let key = MessageKey::new(code);
        assert_eq!(key.russian().as_deref(), Some(*template));
        let _en = ThreadLanguage::set(Lang::En);
        assert_eq!(key.localize().as_deref(), english(template));
        assert_eq!(MessageKey::new("no_such_code").localize(), None);
    }
}
