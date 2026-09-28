use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;

thread_local! {
    static LOGO_IMAGE_CACHE: RefCell<Option<LogoImageCache>> = const { RefCell::new(None) };
}

/// Owns the logo cache for exactly one desktop UI event-loop lifetime.
pub struct LogoCacheSession {
    _thread_bound: PhantomData<Rc<()>>,
}

impl LogoCacheSession {
    pub fn install() -> Self {
        LOGO_IMAGE_CACHE.with(|slot| {
            let previous = slot.borrow_mut().replace(LogoImageCache::new());
            assert!(previous.is_none(), "logo cache session already installed");
        });
        Self {
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for LogoCacheSession {
    fn drop(&mut self) {
        LOGO_IMAGE_CACHE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

/// Runs `f` with the cache installed by the desktop UI event-loop owner.
pub fn with_logo_cache<R>(f: impl FnOnce(&mut LogoImageCache) -> R) -> R {
    LOGO_IMAGE_CACHE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let cache = slot
            .as_mut()
            .expect("logo cache used outside its desktop UI session");
        f(cache)
    })
}

/// Caches successfully decoded logo handles by durable path. A failed
/// attempt is never cached (see `PathImageCache::get_or_load`), so a
/// transient read/decode failure - for example Windows Defender briefly
/// locking a just-written PNG - gets retried on the next lookup instead of
/// silently hiding that logo for the rest of the session.
#[derive(Default)]
pub struct LogoImageCache {
    entries: PathImageCache<slint::Image>,
}

impl LogoImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the decoded image for `path`. A successful decode is cached;
    /// a failed one is retried on the next call instead of being remembered.
    pub fn image_for_path(&mut self, path: impl AsRef<Path>) -> Option<slint::Image> {
        let path = path.as_ref();
        self.entries.get_or_load(path, crate::service_logo::load)
    }

    /// Drops cached images and failures whose paths are no longer referenced.
    pub fn retain_paths<I, P>(&mut self, paths: I)
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        self.entries.retain_paths(paths);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    identity: Option<(u64, u64)>,
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
}

struct PathImageCache<T> {
    entries: HashMap<CacheKey, Option<T>>,
}

impl<T> Default for PathImageCache<T> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }
}

impl<T: Clone> PathImageCache<T> {
    fn get_or_load(&mut self, path: &Path, loader: impl FnOnce(&Path) -> Option<T>) -> Option<T> {
        let key = CacheKey {
            path: path.to_path_buf(),
            identity: file_identity(path),
        };
        if let Some(Some(cached)) = self.entries.get(&key) {
            return Some(cached.clone());
        }
        // Only remember successes. `file_identity` is a no-op on Windows
        // (see below), so a failed attempt has no reliable way to notice a
        // later successful replacement at the same path - retrying instead
        // of caching `None` sidesteps that entirely.
        let loaded = loader(path);
        if loaded.is_some() {
            self.entries.insert(key, loaded.clone());
        }
        loaded
    }

    fn retain_paths<I, P>(&mut self, paths: I)
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let retained: HashSet<PathBuf> = paths
            .into_iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect();
        self.entries.retain(|key, _| retained.contains(&key.path));
    }
}

#[cfg(test)]
mod tests {
    use super::{LogoCacheSession, PathImageCache, with_logo_cache};
    use std::cell::Cell;
    use std::path::Path;

    #[test]
    fn repeated_success_loads_once() {
        let calls = Cell::new(0);
        let mut cache = PathImageCache::default();

        for _ in 0..2 {
            assert_eq!(
                cache.get_or_load(Path::new("logo.png"), |_| {
                    calls.set(calls.get() + 1);
                    Some(7_u8)
                }),
                Some(7)
            );
        }

        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn repeated_failure_retries_every_attempt() {
        // Regression coverage for a real bug report: on Windows, a transient
        // decode failure (for example antivirus briefly locking a
        // just-written PNG) used to be cached forever, so a subscription's
        // logo would silently stay broken for the rest of the session even
        // once the file was perfectly readable again.
        let calls = Cell::new(0);
        let mut cache = PathImageCache::<u8>::default();

        for _ in 0..2 {
            assert_eq!(
                cache.get_or_load(Path::new("missing.png"), |_| {
                    calls.set(calls.get() + 1);
                    None
                }),
                None
            );
        }

        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_transient_failure_heals_once_a_later_attempt_succeeds_and_then_stays_cached() {
        let mut cache = PathImageCache::<u8>::default();
        let path = Path::new("flaky.png");
        let mut attempts = 0;

        assert_eq!(
            cache.get_or_load(path, |_| {
                attempts += 1;
                None
            }),
            None
        );
        assert_eq!(
            cache.get_or_load(path, |_| {
                attempts += 1;
                Some(9_u8)
            }),
            Some(9)
        );
        assert_eq!(
            cache.get_or_load(path, |_| {
                attempts += 1;
                None
            }),
            Some(9),
            "a success must stay cached without re-invoking the loader"
        );
        assert_eq!(attempts, 2);
    }

    #[test]
    fn distinct_paths_are_loaded_independently() {
        let calls = Cell::new(0);
        let mut cache = PathImageCache::default();

        assert_eq!(
            cache.get_or_load(Path::new("one.png"), |_| {
                calls.set(calls.get() + 1);
                Some(1_u8)
            }),
            Some(1)
        );
        assert_eq!(
            cache.get_or_load(Path::new("two.png"), |_| {
                calls.set(calls.get() + 1);
                Some(2_u8)
            }),
            Some(2)
        );

        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn retain_paths_prunes_stale_entries() {
        let calls = Cell::new(0);
        let mut cache = PathImageCache::default();
        let load = |_: &Path| {
            calls.set(calls.get() + 1);
            Some(calls.get())
        };

        assert_eq!(cache.get_or_load(Path::new("keep.png"), load), Some(1));
        assert_eq!(cache.get_or_load(Path::new("drop.png"), load), Some(2));

        cache.retain_paths([Path::new("keep.png")]);

        assert_eq!(cache.get_or_load(Path::new("keep.png"), load), Some(1));
        assert_eq!(cache.get_or_load(Path::new("drop.png"), load), Some(3));
        assert_eq!(calls.get(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_behind_the_same_path_is_loaded_again() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("13");
        let replacement = temp.path().join("replacement");
        std::fs::write(&path, b"first").unwrap();
        let calls = Cell::new(0);
        let mut cache = PathImageCache::default();
        let load = |path: &Path| {
            calls.set(calls.get() + 1);
            std::fs::read(path).ok()
        };

        assert_eq!(cache.get_or_load(&path, load), Some(b"first".to_vec()));
        assert_eq!(cache.get_or_load(&path, load), Some(b"first".to_vec()));
        std::fs::write(&replacement, b"second").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_eq!(cache.get_or_load(&path, load), Some(b"second".to_vec()));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn thread_local_access_reuses_the_same_cache() {
        let _session = LogoCacheSession::install();
        let first = with_logo_cache(std::ptr::from_mut);
        let second = with_logo_cache(std::ptr::from_mut);

        assert_eq!(first, second);
    }

    #[test]
    fn pruning_a_failed_entry_allows_one_new_attempt_when_reintroduced() {
        let calls = Cell::new(0);
        let mut cache = PathImageCache::<u8>::default();
        let path = Path::new("missing.png");

        assert_eq!(
            cache.get_or_load(path, |_| {
                calls.set(calls.get() + 1);
                None
            }),
            None
        );
        cache.retain_paths(std::iter::empty::<&Path>());
        assert_eq!(
            cache.get_or_load(path, |_| {
                calls.set(calls.get() + 1);
                None
            }),
            None
        );
        assert_eq!(calls.get(), 2);
    }
}
