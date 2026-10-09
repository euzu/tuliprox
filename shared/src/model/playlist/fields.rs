use crate::utils::Internable;
use std::sync::Arc;

/// A field's value, borrowed where possible.
///
/// The point of the enum is that reading a field never forces an allocation:
/// the `&str`-keyed accessor had to return `Arc<str>`, so `chno` and `type`
/// went through `.to_string().intern()` — a heap allocation *and* an interner
/// write lock — on every read, per item, per rule.
pub enum FieldRef<'a> {
    /// An interned field. Cloning is a refcount bump.
    Shared(&'a Arc<str>),
    /// A borrowed string that is not interned, e.g. `item_type.as_str()`.
    Str(&'a str),
    /// A numeric field, kept numeric.
    Num(u32),
}

impl FieldRef<'_> {
    /// Borrow as a string, formatting a numeric field into an owned buffer only
    /// when there is one.
    pub fn as_cow(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Shared(value) => std::borrow::Cow::Borrowed(value.as_ref()),
            Self::Str(value) => std::borrow::Cow::Borrowed(value),
            Self::Num(value) => std::borrow::Cow::Owned(value.to_string()),
        }
    }

    /// Materialise as an interned `Arc<str>`.
    ///
    /// This is what the `&str` compatibility shims call, so their behaviour —
    /// including interning numbers — is bit-for-bit what it was before.
    pub fn to_arc(&self) -> Arc<str> {
        match self {
            Self::Shared(value) => Arc::clone(value),
            Self::Str(value) => value.intern(),
            Self::Num(value) => value.to_string().intern(),
        }
    }
}

/// Read a field by typed key. Matches on a discriminant rather than walking a
/// chain of case-insensitive string comparisons.
pub trait FieldGet {
    fn get(&self, field: crate::model::HeaderField) -> Option<FieldRef<'_>>;
}

/// Write a field by typed key.
pub trait FieldSet {
    fn set(&mut self, field: crate::model::HeaderField, value: &str) -> bool;
}

/// Read a field by name.
///
/// Retained for callers whose field name genuinely arrives as a string (the M3U
/// resource endpoint, for one). Implemented as a shim over [`FieldGet`].
pub trait FieldGetAccessor {
    fn get_field(&self, field: &str) -> Option<Arc<str>>;
}

/// Write a field by name. Shim over [`FieldSet`].
pub trait FieldSetAccessor {
    fn set_field(&mut self, field: &str, value: &str) -> bool;
}
