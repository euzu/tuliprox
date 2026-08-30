//! String interning utilities for memory optimization.
//!
//! Provides a global string interner to deduplicate frequently repeated
//! strings like `input_name` and `group` in playlist items.

use crate::model::UUIDType;
use serde::{
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer, Serializer,
};
use std::{
    borrow::Cow,
    collections::HashSet,
    fmt,
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
static INTERNER: LazyLock<RwLock<HashSet<Arc<str>>>> = LazyLock::new(|| RwLock::new(HashSet::new()));

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

macro_rules! intern_impl {
    ($s:expr, $create:expr) => {{
        if let Ok(guard) = INTERNER.read() {
            if let Some(existing) = guard.get($s) {
                return Arc::clone(existing);
            }
        }
        if let Ok(mut guard) = INTERNER.write() {
            if let Some(existing) = guard.get($s) {
                return Arc::clone(existing);
            }
            let arc: Arc<str> = $create;
            guard.insert(Arc::clone(&arc));
            return arc;
        }
        $create
    }};
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

/// Convert an `f64` that reached `visit_f64` into a round-trip-safe string.
///
/// Special values are emitted as `"infinity"`, `"-infinity"`, and `"nan"`.
/// `serde_saphyr` re-serializes these ambiguous scalars quoted, so they
/// survive a YAML round-trip as strings.
///
/// This is a safety-net for paths that intentionally accept typed numeric
/// scalars and normalize them into strings.
#[inline]
fn f64_to_str(v: f64) -> String {
    if v.is_infinite() {
        if v.is_sign_positive() {
            "infinity".to_owned()
        } else {
            "-infinity".to_owned()
        }
    } else if v.is_nan() {
        "nan".to_owned()
    } else {
        v.to_string()
    }
}

#[inline]
// This intentionally only normalizes the lowercase dot-prefixed spellings that
// serde_saphyr emits for special float scalars. Other YAML 1.1 variants such as
// `.Inf`, `.INF`, or `.NaN` are out of scope here because they are not produced
// by the current parser path.
fn normalize_scalar_string(value: &str) -> &str {
    match value {
        ".inf" => "infinity",
        "-.inf" => "-infinity",
        ".nan" => "nan",
        _ => value,
    }
}

/// Returns `true` for values that should be treated as absent.
///
/// Covers:
/// - empty string (`""`)
/// - JSON/YAML null literals (`"null"`, `"~"`)
///
/// Generic, field-agnostic. Callers decide whether `true` maps to `""`
/// (for non-optional `Arc<str>`) or `None` (for `Option<Arc<str>>`).
/// Case-sensitive on purpose: provider-supplied `"NULL"` or `"Null"` are real
/// values and must be preserved verbatim.
#[inline]
pub fn is_nullish(value: &str) -> bool { value.is_empty() || value == "null" || value == "~" }

//
// Two reusable visitor types live here so that multiple public entry-points
// can share them without code duplication:
//
//   ArcStrVisitor        -> Arc<str>         (null/empty -> "")
//   OptionArcStrVisitor  -> Option<Arc<str>> (null/empty -> None)
//
// `ArcStrVisitor::visit_some` uses `deserialize_any(self)`, so numeric/bool
// scalars can flow into the typed `visit_*` methods. The tradeoff is that raw
// numeric notation may be normalized (for example `1e2` becomes `"100"`).
//
// `OptionArcStrVisitor::visit_some` intentionally diverges and uses
// `deserialize_any` so JSON/YAML numeric inputs can flow into `visit_i64`,
// `visit_u64` or `visit_f64`. The tradeoff is that this path no longer forces
// raw-scalar preservation in the same way as `ArcStrVisitor`.

/// Visitor that produces `Arc<str>`, mapping null / empty -> `""`.
struct ArcStrVisitor;

impl<'de> Visitor<'de> for ArcStrVisitor {
    type Value = Arc<str>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("a string, number, boolean, or null") }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok(normalize_scalar_string(v).intern())
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        Ok(normalize_scalar_string(v.as_str()).intern())
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> { Ok(v.to_string().intern()) }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> { Ok(v.to_string().intern()) }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> { Ok(v.to_string().intern()) }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> { Ok(f64_to_str(v).intern()) }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok("".intern()) }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok("".intern()) }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> { d.deserialize_any(self) }
}

/// Visitor that produces `Option<Arc<str>>`, mapping null / empty -> `None`.
struct OptionArcStrVisitor;

impl<'de> Visitor<'de> for OptionArcStrVisitor {
    type Value = Option<Arc<str>>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a string, number, boolean, null, or empty")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        let normalized = normalize_scalar_string(v);
        if normalized.is_empty() {
            Ok(None)
        } else {
            Ok(Some(normalized.intern()))
        }
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        let normalized = normalize_scalar_string(v.as_str());
        if normalized.is_empty() {
            Ok(None)
        } else {
            Ok(Some(normalized.intern()))
        }
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> { Ok(Some(v.to_string().intern())) }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> { Ok(Some(v.to_string().intern())) }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> { Ok(Some(v.to_string().intern())) }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> { Ok(Some(f64_to_str(v).intern())) }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> { d.deserialize_any(self) }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        log::debug!("ignored sequence while deserializing string interner, returning None");
        Ok(None)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        log::debug!("ignored map while deserializing string interner, returning None");
        Ok(None)
    }
}

pub mod arc_str_vec_serde {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use serde::ser::SerializeSeq;

    pub fn serialize<S>(value: &Vec<Arc<str>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(value.len()))?;
        for s in value {
            seq.serialize_element(s.as_ref())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Arc<str>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec = Vec::<String>::deserialize(deserializer)?;
        Ok(vec.into_iter().map(super::Internable::intern).collect())
    }
}

pub mod arc_str_serde {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::*;

    pub fn serialize<S>(value: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(value)
    }

    /// Deserialize a scalar as an interned `Arc<str>`.
    ///
    /// This goes through `deserialize_option(ArcStrVisitor)`, and
    /// `ArcStrVisitor::visit_some` uses `deserialize_any`. That allows numeric
    /// JSON/YAML scalars to be accepted, but raw numeric notation may be lost
    /// during normalization (for example `1e2` becomes `"100"`).
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(ArcStrVisitor)
    }
}

pub mod arc_str_option_serde {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::*;

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
        deserializer.deserialize_option(OptionArcStrVisitor)
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

pub mod arc_str_option_null_if_empty_serde {
    pub use super::arc_str_option_serde::{deserialize, serialize_null_if_empty as serialize};
}

/// Visitor that mirrors `ArcStrVisitor` but collapses `"null"`, `""`, and `~`
/// to an empty `Arc<str>`. Generic over the field, not specific to
/// `container_extension`.
struct NullishArcStrVisitor;

impl<'de> Visitor<'de> for NullishArcStrVisitor {
    type Value = Arc<str>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a string, number, boolean, or null (literal \"null\"/\"~\"/empty mapped to empty)")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        if is_nullish(v) {
            Ok("".intern())
        } else {
            Ok(normalize_scalar_string(v).intern())
        }
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        if is_nullish(&v) {
            Ok("".intern())
        } else {
            Ok(normalize_scalar_string(&v).intern())
        }
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> { Ok(v.to_string().intern()) }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> { Ok(v.to_string().intern()) }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> { Ok(v.to_string().intern()) }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> { Ok(f64_to_str(v).intern()) }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok("".intern()) }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok("".intern()) }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> { d.deserialize_any(self) }
}

/// Visitor that mirrors `OptionArcStrVisitor` but collapses `"null"`, `""`,
/// and `~` to `None`. Generic over the field.
struct NullishOptionArcStrVisitor;

impl<'de> Visitor<'de> for NullishOptionArcStrVisitor {
    type Value = Option<Arc<str>>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a string, number, boolean, null, or empty (literal \"null\"/\"~\"/empty mapped to None)")
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        if is_nullish(v) {
            Ok(None)
        } else {
            Ok(Some(normalize_scalar_string(v).intern()))
        }
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        if is_nullish(&v) {
            Ok(None)
        } else {
            Ok(Some(normalize_scalar_string(&v).intern()))
        }
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> { Ok(Some(v.to_string().intern())) }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> { Ok(Some(v.to_string().intern())) }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> { Ok(Some(v.to_string().intern())) }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> { Ok(Some(f64_to_str(v).intern())) }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> { d.deserialize_any(self) }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        log::debug!("ignored sequence while deserializing nullish arc_str, returning None");
        Ok(None)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        log::debug!("ignored map while deserializing nullish arc_str, returning None");
        Ok(None)
    }
}

/// Generic serde for `Arc<str>` that treats JSON null, YAML null/`~`, empty
/// string, and the literal four-character string `"null"` as empty.
///
/// Field-agnostic. Apply with `#[serde(with = "arc_str_null_is_none_serde")]`.
///
/// Use instead of `arc_str_serde` whenever the underlying value might be
/// reported as `"null"` by a misbehaving provider.
pub mod arc_str_null_is_none_serde {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::*;

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
        deserializer.deserialize_option(NullishArcStrVisitor)
    }
}

/// Generic serde for `Option<Arc<str>>` that treats JSON null, YAML null/`~`,
/// empty string, and the literal four-character string `"null"` as `None`. On
/// serialization, `None`, empty, and the literal `"null"` all become JSON null.
pub mod arc_str_null_is_none_option_serde {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::*;

    pub fn serialize<S>(value: &Option<Arc<str>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(s) if !super::is_nullish(s) => serializer.serialize_str(s),
            _ => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Arc<str>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_option(NullishOptionArcStrVisitor)
    }
}

//
// Reuses `ArcStrVisitor` / `OptionArcStrVisitor` via `deserialize_option`:
//   - null / ~ / empty  -> visit_none / visit_unit -> "" / None
//   - `ArcStrVisitor::visit_some` -> deserialize_any -> numbers/bools map into
//     typed `visit_*` methods, so raw numeric notation may be normalized
//   - `OptionArcStrVisitor::visit_some` -> deserialize_any -> numbers/bools map
//     into their typed `visit_*` methods before being interned as strings

pub use arc_str_default_on_null as arc_str_none_default_on_null;

pub fn arc_str_default_on_null<'de, D>(deserializer: D) -> Result<Arc<str>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(ArcStrVisitor)
}

pub fn deserialize_as_option_arc_str<'de, D>(deserializer: D) -> Result<Option<Arc<str>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_option(OptionArcStrVisitor)
}

#[cfg(test)]
mod tests {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct ArcStrHolder {
        #[serde(default, with = "arc_str_serde")]
        value: Arc<str>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct OptArcStrHolder {
        #[serde(default, with = "arc_str_option_serde")]
        value: Option<Arc<str>>,
    }

    #[test]
    fn arc_str_serde_preserves_yaml_infinity_literal_as_string() {
        let parsed: ArcStrHolder = serde_saphyr::from_str("value: infinity\n").unwrap();
        assert_eq!(parsed.value.as_ref(), "infinity");
    }

    #[test]
    fn arc_str_serde_preserves_yaml_numeric_like_word_as_string() {
        let parsed: ArcStrHolder = serde_saphyr::from_str("value: 01abc\n").unwrap();
        assert_eq!(parsed.value.as_ref(), "01abc");
    }

    #[test]
    fn arc_str_serde_accepts_json_integer() {
        let parsed: ArcStrHolder = serde_json::from_str(r#"{"value":1285728}"#).unwrap();
        assert_eq!(parsed.value.as_ref(), "1285728");
    }

    #[test]
    fn arc_str_option_serde_accepts_json_integer() {
        let parsed: OptArcStrHolder = serde_json::from_str(r#"{"value":8169}"#).unwrap();
        assert_eq!(parsed.value.as_deref(), Some("8169"));
    }

    #[test]
    fn arc_str_option_serde_accepts_json_string() {
        let parsed: OptArcStrHolder = serde_json::from_str(r#"{"value":"8169"}"#).unwrap();
        assert_eq!(parsed.value.as_deref(), Some("8169"));
    }

    #[test]
    fn arc_str_option_serde_maps_empty_string_to_none() {
        let parsed: OptArcStrHolder = serde_json::from_str(r#"{"value":""}"#).unwrap();
        assert_eq!(parsed.value, None);
    }

    #[test]
    fn arc_str_serde_normalizes_json_scientific_notation_numbers() {
        let parsed: ArcStrHolder = serde_json::from_str(r#"{"value":1e2}"#).unwrap();
        assert_eq!(parsed.value.as_ref(), "100");
    }

    #[test]
    fn arc_str_serde_normalizes_yaml_special_float_scalars() {
        let parsed_inf: ArcStrHolder = serde_saphyr::from_str("value: .inf\n").unwrap();
        let parsed_neg_inf: ArcStrHolder = serde_saphyr::from_str("value: -.inf\n").unwrap();
        let parsed_nan: ArcStrHolder = serde_saphyr::from_str("value: .nan\n").unwrap();

        assert_eq!(parsed_inf.value.as_ref(), "infinity");
        assert_eq!(parsed_neg_inf.value.as_ref(), "-infinity");
        assert_eq!(parsed_nan.value.as_ref(), "nan");
    }

    #[test]
    fn arc_str_option_serde_normalizes_yaml_special_float_scalars() {
        let parsed_inf: OptArcStrHolder = serde_saphyr::from_str("value: .inf\n").unwrap();
        let parsed_neg_inf: OptArcStrHolder = serde_saphyr::from_str("value: -.inf\n").unwrap();
        let parsed_nan: OptArcStrHolder = serde_saphyr::from_str("value: .nan\n").unwrap();

        assert_eq!(parsed_inf.value.as_deref(), Some("infinity"));
        assert_eq!(parsed_neg_inf.value.as_deref(), Some("-infinity"));
        assert_eq!(parsed_nan.value.as_deref(), Some("nan"));
    }

    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct NullishArcStrHolder {
        #[serde(default, with = "arc_str_null_is_none_serde")]
        value: Arc<str>,
    }

    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct NullishOptArcStrHolder {
        #[serde(default, with = "arc_str_null_is_none_option_serde")]
        value: Option<Arc<str>>,
    }

    #[test]
    fn null_is_none_arc_str_treats_literal_null_string_as_empty() {
        let parsed: NullishArcStrHolder = serde_json::from_str(r#"{"value":"null"}"#).unwrap();
        assert!(parsed.value.is_empty());
    }

    #[test]
    fn null_is_none_arc_str_treats_yaml_null_scalar_as_empty() {
        let parsed: NullishArcStrHolder = serde_saphyr::from_str("value: null\n").unwrap();
        assert!(parsed.value.is_empty());
        let parsed_tilde: NullishArcStrHolder = serde_saphyr::from_str("value: ~\n").unwrap();
        assert!(parsed_tilde.value.is_empty());
    }

    #[test]
    fn null_is_none_arc_str_treats_json_null_as_empty() {
        let parsed: NullishArcStrHolder = serde_json::from_str(r#"{"value":null}"#).unwrap();
        assert!(parsed.value.is_empty());
    }

    #[test]
    fn null_is_none_arc_str_preserves_real_extensions() {
        for ext in ["mkv", "mp4", "ts", "avi"] {
            let json = format!(r#"{{"value":"{ext}"}}"#);
            let parsed: NullishArcStrHolder = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.value.as_ref(), ext, "extension {ext} must survive");
        }
    }

    #[test]
    fn null_is_none_option_treats_literal_null_string_as_none() {
        let parsed: NullishOptArcStrHolder = serde_json::from_str(r#"{"value":"null"}"#).unwrap();
        assert_eq!(parsed.value, None);
    }

    #[test]
    fn null_is_none_option_treats_json_null_as_none() {
        let parsed: NullishOptArcStrHolder = serde_json::from_str(r#"{"value":null}"#).unwrap();
        assert_eq!(parsed.value, None);
    }

    #[test]
    fn null_is_none_option_treats_empty_string_as_none() {
        let parsed: NullishOptArcStrHolder = serde_json::from_str(r#"{"value":""}"#).unwrap();
        assert_eq!(parsed.value, None);
    }

    #[test]
    fn null_is_none_option_preserves_real_extensions() {
        for ext in ["mkv", "mp4", "ts"] {
            let json = format!(r#"{{"value":"{ext}"}}"#);
            let parsed: NullishOptArcStrHolder = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed.value.as_deref(), Some(ext), "extension {ext} must survive");
        }
    }

    #[test]
    fn null_is_none_option_serializes_none_and_literal_null_as_json_null() {
        let none_value: NullishOptArcStrHolder = NullishOptArcStrHolder { value: None };
        assert_eq!(serde_json::to_string(&none_value).unwrap(), r#"{"value":null}"#);

        let literal_null: NullishOptArcStrHolder = NullishOptArcStrHolder { value: Some("null".into()) };
        assert_eq!(serde_json::to_string(&literal_null).unwrap(), r#"{"value":null}"#);

        let empty: NullishOptArcStrHolder = NullishOptArcStrHolder { value: Some("".into()) };
        assert_eq!(serde_json::to_string(&empty).unwrap(), r#"{"value":null}"#);
    }

    #[test]
    fn null_is_none_arc_str_roundtrips_real_extensions_byte_for_byte() {
        let holder = NullishArcStrHolder { value: "mkv".into() };
        let json = serde_json::to_string(&holder).unwrap();
        assert_eq!(json, r#"{"value":"mkv"}"#);
        let back: NullishArcStrHolder = serde_json::from_str(&json).unwrap();
        assert_eq!(back.value.as_ref(), "mkv");
    }
}
