//! String interning utilities for memory optimization.
//!
//! Provides a global string interner to deduplicate frequently repeated
//! strings like `input_name` and `group` in playlist items.

use crate::model::UUIDType;
use serde::{Deserialize, Deserializer, Serializer};
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock, RwLock};

// Global interner store
static INTERNER: LazyLock<RwLock<HashSet<Arc<str>>>> = LazyLock::new(|| {
    RwLock::new(HashSet::new())
});

pub trait Internable {
    fn intern(self) -> Arc<str>;
}

impl Internable for &Arc<str> {
    fn intern(self) -> Arc<str> {
        Arc::clone(self)
    }
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
    fn intern(self) -> Arc<str> {
        intern_string(self.to_string())
    }
}

impl Internable for String {
    fn intern(self) -> Arc<str> {
        intern_string(self)
    }
}

impl Internable for &String {
    fn intern(self) -> Arc<str> {
        intern_str(self.as_str())
    }
}

impl Internable for &str {
    fn intern(self) -> Arc<str> {
        intern_str(self)
    }
}

impl Internable for u32 {
    fn intern(self) -> Arc<str> {
        intern_string(self.to_string())
    }
}

impl Internable for u64 {
    fn intern(self) -> Arc<str> {
        intern_string(self.to_string())
    }
}

impl Internable for i64 {
    fn intern(self) -> Arc<str> {
        intern_string(self.to_string())
    }
}

/// Interns a string slice.
fn intern_str(s: &str) -> Arc<str> {
    // Try read first
    if let Ok(guard) = INTERNER.read() {
        if let Some(existing) = guard.get(s) {
            return Arc::clone(existing);
        }
        drop(guard);
    }

    // Write lock
    if let Ok(mut guard) = INTERNER.write() {
        // Double check
        if let Some(existing) = guard.get(s) {
            return Arc::clone(existing);
        }

        let arc: Arc<str> = Arc::from(s);
        guard.insert(Arc::clone(&arc));
        return arc;
    }
    Arc::from(s)
}

/// Interns an owned string.
fn intern_string(s: String) -> Arc<str> {
    // Try read first
    if let Ok(guard) = INTERNER.read() {
        if let Some(existing) = guard.get(s.as_str()) {
            return Arc::clone(existing);
        }
        drop(guard);
    }

    // Write lock
    if let Ok(mut guard) = INTERNER.write() {
        // Double check
        if let Some(existing) = guard.get(s.as_str()) {
            return Arc::clone(existing);
        }

        let arc: Arc<str> = Arc::from(s);
        guard.insert(Arc::clone(&arc));
        return arc;
    }
    Arc::from(s)
}

/// Garbage collection: removes strings that are only referenced by the cache.
pub fn interner_gc() -> usize {
    if let Ok(mut guard) = INTERNER.write() {
        let before = guard.len();
        // Arc::strong_count == 1 means the cache is the only one holding it.
        guard.retain(|s| Arc::strong_count(s) > 1);
        let removed = before - guard.len();
        if removed > 0 {
            log::debug!("Pruned {removed} unused interned strings ({} remaining)", guard.len());
        }
        return removed;
    }
    0
}

pub mod arc_str_vec_serde {
    use super::*;
    use serde::ser::SerializeSeq;

    pub fn serialize<S>(
        value: &Vec<Arc<str>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(value.len()))?;
        for s in value {
            seq.serialize_element(s.as_ref())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<Vec<Arc<str>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec = Vec::<String>::deserialize(deserializer)?;
        Ok(vec.into_iter().map(|s| s.intern()).collect())
    }
}

pub mod arc_str_serde {
    use super::*;
    use serde_json::Value;
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
        match Value::deserialize(deserializer)? {
            Value::Null => Ok("".intern()),
            Value::Bool(b) => Ok(b.to_string().intern()),
            Value::Number(n) => Ok(n.to_string().intern()),
            Value::String(v) => Ok(v.intern()),
            Value::Array(_) => Ok("".intern()),
            Value::Object(_) => Ok("".intern()),
        }
    }
}

pub mod arc_str_option_serde {
    use super::*;
    use serde_json::Value;
    pub fn serialize<S>(value: &Option<Arc<str>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(s) => serializer.serialize_str(s),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Arc<str>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt = Option::<Value>::deserialize(deserializer)?;
        match opt {
            Some(value) => match value {
                Value::Bool(b) => Ok(Some(b.to_string().intern())),
                Value::Number(n) => Ok(Some(n.to_string().intern())),
                Value::String(v) => Ok(Some(v.intern())),
                Value::Null | Value::Array(_) | Value::Object(_) => Ok(None),
            },
            None => Ok(None)
        }
    }

    pub fn serialize_null_if_empty<S>(value: &Option<Arc<str>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            None => serializer.serialize_none(),
            Some(s) if s.is_empty() => serializer.serialize_none(),
            Some(s) => serializer.serialize_str(s),
        }
    }
}


pub fn arc_str_default_on_null<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default().intern())
}

pub use arc_str_default_on_null as arc_str_none_default_on_null;

pub fn deserialize_as_option_arc_str<'de, D>(deserializer: D) -> Result<Option<Arc<str>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: serde_json::Value = Deserialize::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(s) => Ok(Some(s.intern())),
        serde_json::Value::Number(s) => Ok(Some(s.to_string().intern())),
        _ => Ok(None),
    }
}
