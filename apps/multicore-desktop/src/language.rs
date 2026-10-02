//! Which interface language the app shows, and who decided it.
//!
//! Russian is the default. A language the user picks in Settings is final: a provider's
//! `app-language` header never overrides it again. Until then the active subscription's
//! header decides (a subscription without one means Russian), and the last language applied
//! is remembered so the next launch starts in it before the daemon answers.
//!
//! The strings themselves come from `multicore_i18n` (`tr!` in Rust, `@tr` in Slint), which
//! read the process-wide language set here.

use std::sync::atomic::{AtomicBool, Ordering};

pub use multicore_i18n::Lang;

/// The user picked the language in Settings (persisted as `language_set_by_user`).
static USER_CHOSEN: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static TEST_USER_CHOSEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The language the app should show.
///
/// `provider` is the active subscription's `app-language` as of the last daemon status:
/// `None` while no status is known yet (the current language stays), `Some(None)` for a
/// status without the header (or without any subscription).
pub fn effective_language(
    user_chosen: bool,
    current: Lang,
    provider: Option<Option<Lang>>,
) -> Lang {
    if user_chosen {
        return current;
    }
    match provider {
        Some(header) => header.unwrap_or(Lang::Ru),
        None => current,
    }
}

/// The language from preferences, applied before any window exists.
pub fn initialize(language: Lang, set_by_user: bool) {
    set_user_chosen(set_by_user);
    set_language(language);
}

pub fn user_chosen() -> bool {
    #[cfg(test)]
    {
        TEST_USER_CHOSEN.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        USER_CHOSEN.load(Ordering::Relaxed)
    }
}

fn set_user_chosen(chosen: bool) {
    #[cfg(test)]
    TEST_USER_CHOSEN.with(|cell| cell.set(chosen));
    #[cfg(not(test))]
    USER_CHOSEN.store(chosen, Ordering::Relaxed);
}

pub fn current() -> Lang {
    multicore_i18n::current()
}

/// Sets the language strings are produced in. Tests keep it per thread, so parallel tests
/// never see each other's language.
fn set_language(language: Lang) {
    #[cfg(test)]
    std::mem::forget(multicore_i18n::ThreadLanguage::set(language));
    #[cfg(not(test))]
    multicore_i18n::set_current(language);
}

/// The user picked `language` in Settings: from now on it is final.
pub fn choose(language: Lang) {
    set_user_chosen(true);
    set_language(language);
}

/// A daemon status arrived; `provider` is its subscription's `app-language` (see
/// [`effective_language`]). Returns whether the language changed.
pub fn apply_provider_language(provider: Option<Option<Lang>>) -> bool {
    let current = current();
    let next = effective_language(user_chosen(), current, provider);
    if next == current {
        return false;
    }
    set_language(next);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_is_the_default_without_a_header_or_a_choice() {
        assert_eq!(effective_language(false, Lang::Ru, Some(None)), Lang::Ru);
        // A subscription without the header puts a provider-set English back to Russian.
        assert_eq!(effective_language(false, Lang::En, Some(None)), Lang::Ru);
        assert_eq!(Lang::default(), Lang::Ru);
    }

    #[test]
    fn the_header_applies_until_the_user_chooses() {
        assert_eq!(
            effective_language(false, Lang::Ru, Some(Some(Lang::En))),
            Lang::En
        );
        assert_eq!(
            effective_language(false, Lang::En, Some(Some(Lang::Ru))),
            Lang::Ru
        );
        // No status yet: keep what the last launch applied.
        assert_eq!(effective_language(false, Lang::En, None), Lang::En);
    }

    #[test]
    fn a_user_choice_beats_every_header() {
        assert_eq!(
            effective_language(true, Lang::Ru, Some(Some(Lang::En))),
            Lang::Ru
        );
        assert_eq!(
            effective_language(true, Lang::En, Some(Some(Lang::Ru))),
            Lang::En
        );
        assert_eq!(effective_language(true, Lang::En, Some(None)), Lang::En);
    }

    #[test]
    fn provider_language_switches_live_until_the_user_chooses() {
        initialize(Lang::Ru, false);
        assert!(apply_provider_language(Some(Some(Lang::En))));
        assert_eq!(current(), Lang::En);
        assert_eq!(
            multicore_i18n::tr!("Выбран провайдером подписки"),
            "Set by your subscription provider"
        );
        assert!(!apply_provider_language(Some(Some(Lang::En))));
        assert!(!apply_provider_language(None));
        assert_eq!(current(), Lang::En);
        assert!(apply_provider_language(Some(None)));
        assert_eq!(current(), Lang::Ru);

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
    fn a_saved_user_choice_survives_a_restart() {
        initialize(Lang::En, true);
        assert!(user_chosen());
        assert!(!apply_provider_language(Some(Some(Lang::Ru))));
        assert_eq!(current(), Lang::En);
    }
}
