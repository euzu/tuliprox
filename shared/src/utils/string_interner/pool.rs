use crate::model::UUIDType;
use std::{
    borrow::Cow,
    collections::HashSet,
    sync::{Arc, LazyLock, RwLock},
};

/// Global interning pool.
///
/// ## Performance (millions of entries)
///
/// * **Happy-path (already-interned):** one `RwLock::read()` + hash lookup +
///   `Arc::clone` (single atomic increment).  Multiple threads can read
///   concurrently without blocking each other.
/// * **First-time intern:** upgrades to a write lock, double-checks, and
///   inserts.  This only happens *once per unique string value*, so the write
///   path is not on the hot parse loop.
/// * **`deserialize_string` vs `deserialize_any`:** the former is *faster*
///   because saphyr skips bool / int / float parsing attempts and hands the
///   raw scalar text directly to the visitor.
/// * **Pruning:** call `interner_gc()` periodically (e.g. after a full
///   playlist reload) to release strings that are only referenced by the
///   pool itself.
pub(super) static INTERNER: LazyLock<RwLock<HashSet<Arc<str>>>> = LazyLock::new(|| RwLock::new(HashSet::new()));

pub trait Internable {
    fn intern(self) -> Arc<str>;
}

impl Internable for &Arc<str> {
    fn intern(self) -> Arc<str> { Arc::clone(self) }
}

impl Internable for &Cow<'_, str> {
    fn intern(self) -> Arc<str> {
        match self {
            Cow::Borrowed(s) => intern_str(s),
            Cow::Owned(s) => intern_string(s.clone()),
        }
    }
}

impl Internable for &UUIDType {
    fn intern(self) -> Arc<str> { intern_string(self.to_string()) }
}

impl Internable for String {
    fn intern(self) -> Arc<str> { intern_string(self) }
}

impl Internable for &String {
    fn intern(self) -> Arc<str> { intern_str(self.as_str()) }
}

impl Internable for &str {
    fn intern(self) -> Arc<str> { intern_str(self) }
}

impl Internable for u32 {
    fn intern(self) -> Arc<str> { intern_string(self.to_string()) }
}

impl Internable for u64 {
    fn intern(self) -> Arc<str> { intern_string(self.to_string()) }
}

impl Internable for i64 {
    fn intern(self) -> Arc<str> { intern_string(self.to_string()) }
}

/// Interns a string slice.
fn intern_str(s: &str) -> Arc<str> { intern_impl!(s, Arc::from(s)) }

/// Interns an owned string.
fn intern_string(s: String) -> Arc<str> { intern_impl!(s.as_str(), Arc::from(s)) }

/// Returns the current number of strings held in the interning pool.
/// Uses a read lock and is safe to call on hot paths for threshold checks.
pub fn interner_len() -> usize { INTERNER.read().map_or(0, |g| g.len()) }

/// Garbage collection: removes strings that are only referenced by the cache.
pub fn interner_gc() -> usize {
    if let Ok(mut guard) = INTERNER.write() {
        let before = guard.len();
        guard.retain(|s| Arc::strong_count(s) > 1);
        let removed = before - guard.len();
        if removed > 0 {
            log::debug!("Pruned {removed} unused interned strings ({} remaining)", guard.len());
        }
        return removed;
    }
    0
}
