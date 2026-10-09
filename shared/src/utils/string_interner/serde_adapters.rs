use super::{
    is_nullish,
    scalar::{f64_to_str, normalize_scalar_string},
    Internable,
};
use serde::{
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
    Deserializer,
};
use std::{fmt, sync::Arc};

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
pub(super) struct ArcStrVisitor;

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
pub(super) struct OptionArcStrVisitor;

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
    use super::super::*;
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
        Ok(vec.into_iter().map(super::super::Internable::intern).collect())
    }
}

pub mod arc_str_serde {
    // These serde helper modules deliberately re-use the parent module's imports.
    // Clippy's expansion of this glob names private sibling modules and does not
    // compile, so the glob is kept.
    #[allow(clippy::wildcard_imports)]
    use super::super::*;

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
    use super::super::*;

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
    pub use super::super::arc_str_option_serde::{deserialize, serialize_null_if_empty as serialize};
}

/// Visitor that mirrors `ArcStrVisitor` but collapses `"null"`, `""`, and `~`
/// to an empty `Arc<str>`. Generic over the field, not specific to
/// `container_extension`.
pub(super) struct NullishArcStrVisitor;

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
pub(super) struct NullishOptionArcStrVisitor;

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
    use super::super::*;

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
    use super::super::*;

    pub fn serialize<S>(value: &Option<Arc<str>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(s) if !super::super::is_nullish(s) => serializer.serialize_str(s),
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
