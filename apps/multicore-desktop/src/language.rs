//! Which interface language the app shows, and who decided it.
//!
//! Precedence: (1) a language the user picked in Settings — final, provider headers never
//! override it again; else (2) the active subscription's `app-language` header; else (3) the
//! system UI language (Russian for ru/be/kk/ky/tg/uz, English for everything else, Russian when
//! it cannot be read). The system language is read at startup and never saved as a choice.
//!
//! A provider's language is remembered in preferences so the next launch starts in it before
//! the daemon answers. The strings themselves come from `multicore_i18n` (`tr!` in Rust, `@tr`
//! in Slint), which read the process-wide language set here.

#[cfg(not(test))]
use std::sync::atomic::{AtomicU8, Ordering};

pub use multicore_i18n::Lang;

/// Who decided the current language.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// Picked in Settings (persisted as `language_set_by_user`).
    User,
    /// The active subscription's `app-language` header (or remembered from the last launch).
    Provider,
    /// The system UI language.
    System,
}

#[cfg(not(test))]
static SOURCE: AtomicU8 = AtomicU8::new(2);
#[cfg(not(test))]
static SYSTEM: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
thread_local! {
    static TEST_STATE: std::cell::Cell<(Source, Lang)> =
        const { std::cell::Cell::new((Source::System, Lang::Ru)) };
}

/// The language the app should show.
///
/// `provider` is the active subscription's `app-language` as of the last daemon status:
/// `None` while no status is known yet (the current language stays), `Some(None)` for a
/// status without the header (or without any subscription), which falls back to `system`.
pub fn effective_language(
    user_chosen: bool,
    current: Lang,
    provider: Option<Option<Lang>>,
    system: Lang,
) -> Lang {
    if user_chosen {
        return current;
    }
    match provider {
        Some(header) => header.unwrap_or(system),
        None => current,
    }
}

/// The interface language for a system language tag (`ru-RU`, `kk_KZ.UTF-8`, `uk`): Russian
/// for the languages whose users read Russian more readily than English, English otherwise.
/// `None` when the tag names no language (`C`, `POSIX`, empty).
pub fn language_for_system_tag(tag: &str) -> Option<Lang> {
    let base = tag
        .trim()
        .split(['-', '_', '.', '@'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match base.as_str() {
        "" | "c" | "posix" => None,
        "ru" | "be" | "kk" | "ky" | "tg" | "uz" => Some(Lang::Ru),
        _ if base.chars().all(|c| c.is_ascii_alphabetic()) => Some(Lang::En),
        _ => None,
    }
}

/// The system UI language; Russian when it cannot be read.
pub fn detect_system_language() -> Lang {
    system_language_tag()
        .as_deref()
        .and_then(language_for_system_tag)
        .unwrap_or(Lang::Ru)
}

#[cfg(windows)]
fn system_language_tag() -> Option<String> {
    use windows_sys::Win32::Globalization::{
        GetUserDefaultUILanguage, GetUserPreferredUILanguages, MUI_LANGUAGE_NAME,
    };
    let mut count = 0_u32;
    let mut length = 0_u32;
    // SAFETY: a size query with a null buffer; both out pointers are valid.
    let sized = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if sized != 0 && length > 0 && length < 4096 {
        let mut buffer = vec![0_u16; length as usize];
        // SAFETY: `buffer` holds `length` UTF-16 units as the size query asked for.
        let filled = unsafe {
            GetUserPreferredUILanguages(
                MUI_LANGUAGE_NAME,
                &mut count,
                buffer.as_mut_ptr(),
                &mut length,
            )
        };
        if filled != 0 {
            let first = buffer.split(|unit| *unit == 0).next().unwrap_or_default();
            let tag = String::from_utf16_lossy(first);
            if !tag.is_empty() {
                return Some(tag);
            }
        }
    }
    // SAFETY: no arguments; returns a LANGID.
    let langid = unsafe { GetUserDefaultUILanguage() };
    primary_language_tag(langid).map(str::to_owned)
}

/// The base language of a Windows LANGID (its low 10 bits) for the languages that matter here.
#[cfg(any(windows, test))]
fn primary_language_tag(langid: u16) -> Option<&'static str> {
    Some(match langid & 0x03ff {
        0x00 => return None,
        0x19 => "ru",
        0x23 => "be",
        0x3f => "kk",
        0x40 => "ky",
        0x28 => "tg",
        0x43 => "uz",
        _ => "en",
    })
}

#[cfg(not(windows))]
fn system_language_tag() -> Option<String> {
    system_language_tag_from(|name| std::env::var(name).ok())
}

/// gettext's order: `LANGUAGE` (its first entry) wins unless the locale is `C`/`POSIX`;
/// the locale is `LC_ALL`, else `LC_MESSAGES`, else `LANG`.
#[cfg(any(not(windows), test))]
fn system_language_tag_from(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    let set = |name: &str| var(name).filter(|value| !value.trim().is_empty());
    let locale = set("LC_ALL")
        .or_else(|| set("LC_MESSAGES"))
        .or_else(|| set("LANG"));
    let plain_c = locale
        .as_deref()
        .is_some_and(|locale| language_for_system_tag(locale).is_none());
    if !plain_c
        && let Some(first) = set("LANGUAGE").and_then(|list| {
            list.split(':')
                .find(|entry| !entry.is_empty())
                .map(str::to_owned)
        })
    {
        return Some(first);
    }
    locale
}

/// Applies the saved preference and the system language before any window exists.
pub fn initialize(saved: Lang, set_by_user: bool, system: Lang) {
    set_system(system);
    let (source, language) = if set_by_user {
        (Source::User, saved)
    } else if saved != Lang::default() {
        // Only a provider's language is ever saved without a user choice.
        (Source::Provider, saved)
    } else {
        (Source::System, system)
    };
    set_state(source, language);
}

pub fn user_chosen() -> bool {
    source() == Source::User
}

pub fn current() -> Lang {
    multicore_i18n::current()
}

/// What to save: the language unless it only follows the system (that is read anew at every
/// start), and whether the user picked it.
pub fn persisted() -> (Lang, bool) {
    match source() {
        Source::User => (current(), true),
        Source::Provider => (current(), false),
        Source::System => (Lang::default(), false),
    }
}

/// The user picked `language` in Settings: from now on it is final.
pub fn choose(language: Lang) {
    set_state(Source::User, language);
}

/// A daemon status arrived; `provider` is its subscription's `app-language` (see
/// [`effective_language`]). Returns whether the language changed.
pub fn apply_provider_language(provider: Option<Option<Lang>>) -> bool {
    let current = current();
    if user_chosen() {
        return false;
    }
    let Some(header) = provider else {
        return false;
    };
    let next = effective_language(false, current, provider, system());
    set_state(
        if header.is_some() {
            Source::Provider
        } else {
            Source::System
        },
        next,
    );
    next != current
}

pub fn source() -> Source {
    #[cfg(test)]
    {
        TEST_STATE.with(std::cell::Cell::get).0
    }
    #[cfg(not(test))]
    {
        match SOURCE.load(Ordering::Relaxed) {
            0 => Source::User,
            1 => Source::Provider,
            _ => Source::System,
        }
    }
}

fn system() -> Lang {
    #[cfg(test)]
    {
        TEST_SYSTEM.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        match SYSTEM.load(Ordering::Relaxed) {
            1 => Lang::En,
            _ => Lang::Ru,
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_SYSTEM: std::cell::Cell<Lang> = const { std::cell::Cell::new(Lang::Ru) };
}

fn set_system(language: Lang) {
    #[cfg(test)]
    TEST_SYSTEM.with(|cell| cell.set(language));
    #[cfg(not(test))]
    SYSTEM.store(u8::from(language == Lang::En), Ordering::Relaxed);
}

/// Sets the language strings are produced in. Tests keep it per thread, so parallel tests
/// never see each other's language.
fn set_state(source: Source, language: Lang) {
    #[cfg(test)]
    {
        TEST_STATE.with(|cell| cell.set((source, language)));
        std::mem::forget(multicore_i18n::ThreadLanguage::set(language));
    }
    #[cfg(not(test))]
    {
        SOURCE.store(
            match source {
                Source::User => 0,
                Source::Provider => 1,
                Source::System => 2,
            },
            Ordering::Relaxed,
        );
        multicore_i18n::set_current(language);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_matrix() {
        use Lang::{En, Ru};
        // (user chose, current, provider status, system) -> shown
        for (user, current, provider, system, shown) in [
            // No choice, no header: the system language.
            (false, Ru, Some(None), Ru, Ru),
            (false, Ru, Some(None), En, En),
            (false, En, Some(None), Ru, Ru),
            // No choice: the header wins over the system.
            (false, Ru, Some(Some(En)), Ru, En),
            (false, En, Some(Some(Ru)), En, Ru),
            // No status yet: keep what the start applied.
            (false, En, None, Ru, En),
            (false, Ru, None, En, Ru),
            // The user's choice beats header and system.
            (true, Ru, Some(Some(En)), En, Ru),
            (true, En, Some(Some(Ru)), Ru, En),
            (true, En, Some(None), Ru, En),
            (true, Ru, None, En, Ru),
        ] {
            assert_eq!(
                effective_language(user, current, provider, system),
                shown,
                "user={user} current={current} provider={provider:?} system={system}"
            );
        }
    }

    #[test]
    fn system_tags_map_to_russian_or_english() {
        for (tag, expected) in [
            ("ru-RU", Some(Lang::Ru)),
            ("RU", Some(Lang::Ru)),
            ("be-BY", Some(Lang::Ru)),
            ("kk_KZ.UTF-8", Some(Lang::Ru)),
            ("ky-KG", Some(Lang::Ru)),
            ("tg-Cyrl-TJ", Some(Lang::Ru)),
            ("uz-Latn-UZ", Some(Lang::Ru)),
            ("ru_RU.UTF-8@euro", Some(Lang::Ru)),
            ("uk-UA", Some(Lang::En)),
            ("en-US", Some(Lang::En)),
            ("de_DE.UTF-8", Some(Lang::En)),
            ("zh-Hans-CN", Some(Lang::En)),
            ("C", None),
            ("POSIX", None),
            ("C.UTF-8", None),
            ("", None),
        ] {
            assert_eq!(language_for_system_tag(tag), expected, "{tag:?}");
        }
    }

    #[test]
    fn windows_language_ids_map_by_primary_language() {
        assert_eq!(primary_language_tag(0x0419), Some("ru"));
        assert_eq!(primary_language_tag(0x0423), Some("be"));
        assert_eq!(primary_language_tag(0x043f), Some("kk"));
        assert_eq!(primary_language_tag(0x0440), Some("ky"));
        assert_eq!(primary_language_tag(0x0428), Some("tg"));
        assert_eq!(primary_language_tag(0x0443), Some("uz"));
        assert_eq!(primary_language_tag(0x0843), Some("uz"));
        assert_eq!(
            primary_language_tag(0x0422),
            Some("en"),
            "Ukrainian reads English"
        );
        assert_eq!(primary_language_tag(0x0409), Some("en"));
        assert_eq!(primary_language_tag(0), None);
    }

    #[test]
    fn linux_locale_follows_gettext_order() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(
            system_language_tag_from(env(&[("LANG", "ru_RU.UTF-8")])).as_deref(),
            Some("ru_RU.UTF-8")
        );
        assert_eq!(
            system_language_tag_from(env(&[("LANG", "ru_RU.UTF-8"), ("LC_MESSAGES", "en_US")]))
                .as_deref(),
            Some("en_US")
        );
        assert_eq!(
            system_language_tag_from(env(&[("LC_ALL", "de_DE"), ("LC_MESSAGES", "ru_RU")]))
                .as_deref(),
            Some("de_DE")
        );
        assert_eq!(
            system_language_tag_from(env(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "kk:ru:en")]))
                .as_deref(),
            Some("kk")
        );
        // LANGUAGE is ignored for the C locale.
        assert_eq!(
            system_language_tag_from(env(&[("LANG", "C"), ("LANGUAGE", "en")])).as_deref(),
            Some("C")
        );
        assert_eq!(system_language_tag_from(env(&[])), None);
    }

    #[test]
    fn startup_uses_the_choice_then_a_remembered_provider_language_then_the_system() {
        initialize(Lang::Ru, true, Lang::En);
        assert_eq!((current(), source()), (Lang::Ru, Source::User));
        assert_eq!(persisted(), (Lang::Ru, true));

        initialize(Lang::En, false, Lang::Ru);
        assert_eq!((current(), source()), (Lang::En, Source::Provider));
        assert_eq!(persisted(), (Lang::En, false));

        initialize(Lang::Ru, false, Lang::En);
        assert_eq!((current(), source()), (Lang::En, Source::System));
        // The system language is read anew at every start, never saved.
        assert_eq!(persisted(), (Lang::Ru, false));
    }

    #[test]
    fn provider_language_switches_live_until_the_user_chooses() {
        initialize(Lang::Ru, false, Lang::Ru);
        assert!(apply_provider_language(Some(Some(Lang::En))));
        assert_eq!((current(), source()), (Lang::En, Source::Provider));
        assert_eq!(
            multicore_i18n::tr!("Выбран провайдером подписки"),
            "Set by your subscription provider"
        );
        assert!(!apply_provider_language(Some(Some(Lang::En))));
        assert!(!apply_provider_language(None));
        assert_eq!(current(), Lang::En);
        // The subscription lost its header: back to the system language.
        assert!(apply_provider_language(Some(None)));
        assert_eq!((current(), source()), (Lang::Ru, Source::System));

        choose(Lang::En);
        assert!(user_chosen());
        assert!(!apply_provider_language(Some(Some(Lang::Ru))));
        assert!(!apply_provider_language(Some(None)));
        assert_eq!(current(), Lang::En);
        choose(Lang::Ru);
        assert_eq!(current(), Lang::Ru);
        assert_eq!(
            multicore_i18n::tr!("Выбран провайдером подписки"),
            "Выбран провайдером подписки"
        );
    }

    #[test]
    fn an_english_system_without_a_header_shows_english() {
        initialize(Lang::Ru, false, Lang::En);
        assert_eq!(current(), Lang::En);
        assert!(!apply_provider_language(Some(None)));
        assert_eq!(current(), Lang::En);
        assert!(apply_provider_language(Some(Some(Lang::Ru))));
        assert_eq!(current(), Lang::Ru);
    }

    #[test]
    fn a_saved_user_choice_survives_a_restart() {
        initialize(Lang::En, true, Lang::Ru);
        assert!(user_chosen());
        assert!(!apply_provider_language(Some(Some(Lang::Ru))));
        assert_eq!(current(), Lang::En);
    }
}
