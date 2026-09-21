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

/// Caches decoded logo handles, including failed decode attempts, by durable path.
#[derive(Default)]
pub struct LogoImageCache {
    entries: PathImageCache<slint::Image>,
}

impl LogoImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the decoded image for `path`, loading it only on the first lookup.
    pub fn image_for_path(&mut self, path: impl AsRef<Path>) -> Option<slint::Image> {
        let path = path.as_ref();
        self.entries
            .get_or_load(path, |path| slint::Image::load_from_path(path).ok())
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

struct PathImageCache<T> {
    entries: HashMap<PathBuf, Option<T>>,
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
        self.entries
            .entry(path.to_path_buf())
            .or_insert_with(|| loader(path))
            .clone()
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
        self.entries.retain(|path, _| retained.contains(path));
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
    fn repeated_failure_loads_once() {
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

        assert_eq!(calls.get(), 1);
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
