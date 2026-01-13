//! String interning utilities for memory optimization.
//!
//! Provides a scoped string interner to deduplicate frequently repeated
//! strings like `input_name` and `group` in playlist items.

use std::collections::HashSet;
use std::sync::Arc;

/// A scoped string interner that deduplicates strings within its lifetime.
/// Dropping this struct releases all internal references.
#[derive(Default)]
pub struct StringInterner {
    pool: HashSet<Arc<str>>,
}

impl StringInterner {
    /// Creates a new empty interner.
    pub fn new() -> Self {
        Self {
            pool: HashSet::new(),
        }
    }

    /// Interns a string slice.
    pub fn intern(&mut self, s: &str) -> Arc<str> {
        if let Some(existing) = self.pool.get(s) {
            Arc::clone(existing)
        } else {
            let arc: Arc<str> = s.into();
            self.pool.insert(Arc::clone(&arc));
            arc
        }
    }

    /// Interns an owned string.
    pub fn intern_string(&mut self, s: String) -> Arc<str> {
        if let Some(existing) = self.pool.get(s.as_str()) {
            return Arc::clone(existing);
        }
        // Convert String to Arc<str> directly
        let arc: Arc<str> = Arc::from(s);
        self.pool.insert(Arc::clone(&arc));
        arc
    }
}

/// Legacy wrapper: creates a new Arc<str> without interning.
/// Safe fallback to prevent memory leaks from global/thread-local storage.
#[inline]
pub fn intern(s: &str) -> Arc<str> {
    s.into()
}

/// Legacy wrapper: creates a new Arc<str> without interning.
#[inline]
pub fn intern_string(s: String) -> Arc<str> {
    s.into()
}

/// Serde support for `Arc<str>` fields.
/// Note: This does NOT deduplicate on load to avoid global state leaks.
pub mod arc_str_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::Arc;

    pub fn serialize<S>(value: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(s.into())
    }
}

