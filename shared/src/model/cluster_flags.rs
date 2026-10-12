use crate::{
    create_bitset,
    model::{PlaylistItemType, XtreamCluster},
};
use serde::{
    de,
    de::{Error, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize, Serializer,
};
use std::fmt;

create_bitset!(u8, ClusterFlag, Live, Vod, Series);

/// Which xtream clusters (live, vod, series) something applies to.
///
/// A newtype over the `create_bitset!` set rather than the set itself: the
/// generated set derives a plain `u8` serde form, while this type's wire form
/// is the `[live,vod]` string persisted in configs and user databases. The API
/// mirrors the `bitflags` type it replaced, so call sites read the same.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct ClusterFlags(ClusterFlagSet);

#[allow(non_upper_case_globals)]
impl ClusterFlags {
    pub const Live: Self = Self::flag(ClusterFlag::Live);
    pub const Vod: Self = Self::flag(ClusterFlag::Vod);
    pub const Series: Self = Self::flag(ClusterFlag::Series);

    const fn flag(flag: ClusterFlag) -> Self { Self(ClusterFlagSet(1 << flag as u8)) }
}

impl Default for ClusterFlags {
    fn default() -> Self { Self::all() }
}

impl ClusterFlags {
    #[must_use]
    pub const fn all() -> Self { Self(ClusterFlagSet::ALL) }

    #[must_use]
    pub const fn empty() -> Self { Self(ClusterFlagSet::new()) }

    /// The raw bits, widened to the `u16` the `bitflags` type exposed.
    #[inline]
    pub fn bits(&self) -> u16 { u16::from(self.0 .0) }

    #[inline]
    pub fn is_all(&self) -> bool { self.0 == ClusterFlagSet::ALL }

    #[inline]
    pub fn is_empty(&self) -> bool { self.0.is_empty() }

    /// Whether every cluster in `other` is set.
    #[inline]
    pub fn contains(&self, other: Self) -> bool { self.0.contains_all(&other.0) }

    /// Whether any cluster in `other` is set.
    #[inline]
    pub fn intersects(&self, other: Self) -> bool { self.0.contains_any(&other.0) }

    #[inline]
    pub fn insert(&mut self, other: Self) { self.0.union(other.0); }

    #[inline]
    pub fn remove(&mut self, other: Self) { self.0 = ClusterFlagSet(self.0 .0 & !other.0 .0); }

    #[inline]
    pub fn toggle(&mut self, other: Self) { self.0 = ClusterFlagSet(self.0 .0 ^ other.0 .0); }

    #[inline]
    pub fn set(&mut self, other: Self, value: bool) {
        if value {
            self.insert(other);
        } else {
            self.remove(other);
        }
    }

    pub fn has_cluster(&self, item_type: PlaylistItemType) -> bool {
        self.0.contains(match item_type.cluster() {
            XtreamCluster::Live => ClusterFlag::Live,
            XtreamCluster::Video => ClusterFlag::Vod,
            XtreamCluster::Series => ClusterFlag::Series,
        })
    }

    pub fn has_full_flags(&self) -> bool { self.is_all() }

    fn parse_flag(item: &str) -> Result<ClusterFlag, String> {
        let item = item.trim();
        // ASCII-insensitive compare: no lowercase copy per item.
        [("live", ClusterFlag::Live), ("vod", ClusterFlag::Vod), ("series", ClusterFlag::Series)]
            .into_iter()
            .find_map(|(name, flag)| name.eq_ignore_ascii_case(item).then_some(flag))
            .ok_or_else(|| format!("Invalid flag {}, allowed are live, vod, series", item.to_lowercase()))
    }

    fn from_items<I, S>(items: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut result = ClusterFlagSet::new();
        for item in items {
            result.set(Self::parse_flag(item.as_ref())?);
        }
        Ok(Self(result))
    }
}

impl std::ops::BitOr for ClusterFlags {
    type Output = Self;

    #[inline]
    fn bitor(self, other: Self) -> Self { Self(ClusterFlagSet(self.0 .0 | other.0 .0)) }
}

impl std::ops::BitOrAssign for ClusterFlags {
    #[inline]
    fn bitor_assign(&mut self, other: Self) { self.insert(other); }
}

impl From<Option<ClusterFlags>> for ClusterFlags {
    fn from(value: Option<ClusterFlags>) -> Self { value.unwrap_or_else(ClusterFlags::all) }
}

impl fmt::Display for ClusterFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        let mut separator = "";
        for (flag, name) in [(ClusterFlag::Live, "live"), (ClusterFlag::Vod, "vod"), (ClusterFlag::Series, "series")] {
            if self.0.contains(flag) {
                f.write_str(separator)?;
                f.write_str(name)?;
                separator = ",";
            }
        }
        f.write_str("]")
    }
}

impl fmt::Debug for ClusterFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "ClusterFlags({self})") }
}

impl TryFrom<&str> for ClusterFlags {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let input = value.trim().trim_matches(|c| ['[', ']', '(', ')'].contains(&c));
        if input.is_empty() {
            return Ok(Self::empty());
        }
        let items = input.split(',').map(str::trim);
        ClusterFlags::from_items(items)
    }
}

impl TryFrom<Vec<String>> for ClusterFlags {
    type Error = String;

    fn try_from(value: Vec<String>) -> Result<Self, Self::Error> { ClusterFlags::from_items(value) }
}

impl Serialize for ClusterFlags {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_some(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ClusterFlags {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ClusterFlagsVisitor;

        impl<'de> Visitor<'de> for ClusterFlagsVisitor {
            type Value = ClusterFlags;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string or a map entry like : [vod, live, series]")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                ClusterFlags::try_from(v).map_err(E::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut result = ClusterFlagSet::new();
                while let Some(value) = seq.next_element::<String>()? {
                    result.set(ClusterFlags::parse_flag(&value).map_err(A::Error::custom)?);
                }
                Ok(ClusterFlags(result))
            }
        }
        deserializer.deserialize_any(ClusterFlagsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::ClusterFlags;
    use crate::model::PlaylistItemType;

    #[test]
    fn default_and_none_mean_all_clusters() {
        assert_eq!(ClusterFlags::default(), ClusterFlags::all());
        assert_eq!(ClusterFlags::from(None), ClusterFlags::all());
        assert!(ClusterFlags::all().is_all());
        assert!(ClusterFlags::all().has_full_flags());
        assert!(ClusterFlags::empty().is_empty());
        assert!(!(ClusterFlags::Live | ClusterFlags::Vod).is_all());
    }

    #[test]
    fn set_operations_behave_like_flags() {
        let mut flags = ClusterFlags::empty();
        flags.insert(ClusterFlags::Live);
        flags |= ClusterFlags::Series;
        assert!(flags.contains(ClusterFlags::Live));
        assert!(flags.contains(ClusterFlags::Live | ClusterFlags::Series));
        assert!(!flags.contains(ClusterFlags::Live | ClusterFlags::Vod));
        assert!(flags.intersects(ClusterFlags::Live | ClusterFlags::Vod));
        assert!(!flags.intersects(ClusterFlags::Vod));
        flags.set(ClusterFlags::Live, false);
        flags.set(ClusterFlags::Vod, true);
        assert_eq!(flags, ClusterFlags::Vod | ClusterFlags::Series);
        flags.toggle(ClusterFlags::Series);
        flags.toggle(ClusterFlags::Live);
        assert_eq!(flags, ClusterFlags::Live | ClusterFlags::Vod);
    }

    #[test]
    fn has_cluster_maps_item_types() {
        let flags = ClusterFlags::Vod;
        assert!(flags.has_cluster(PlaylistItemType::Video));
        assert!(!flags.has_cluster(PlaylistItemType::Live));
    }

    #[test]
    fn display_and_parse_round_trip() {
        assert_eq!((ClusterFlags::Live | ClusterFlags::Series).to_string(), "[live,series]");
        assert_eq!(ClusterFlags::empty().to_string(), "[]");
        assert_eq!(ClusterFlags::try_from("[ Vod , live ]"), Ok(ClusterFlags::Live | ClusterFlags::Vod));
        assert_eq!(ClusterFlags::try_from("()"), Ok(ClusterFlags::empty()));
        assert!(ClusterFlags::try_from("live,radio").is_err());
        assert_eq!(ClusterFlags::try_from(vec!["series".to_string()]), Ok(ClusterFlags::Series));
    }

    /// The serialized form is persisted in configs and user databases, so it
    /// must not change with the implementation underneath.
    #[test]
    fn serde_format_is_stable() {
        let flags = ClusterFlags::Live | ClusterFlags::Vod;
        assert_eq!(serde_json::to_string(&flags).unwrap(), r#""[live,vod]""#);
        assert_eq!(serde_json::from_str::<ClusterFlags>(r#""[live,vod]""#).unwrap(), flags);
        assert_eq!(serde_json::from_str::<ClusterFlags>(r#"["LIVE"," vod "]"#).unwrap(), flags);
        assert_eq!(serde_saphyr::from_str::<ClusterFlags>("[series]\n").unwrap(), ClusterFlags::Series);
        let bin = rmp_serde::to_vec(&flags).unwrap();
        assert_eq!(rmp_serde::from_slice::<ClusterFlags>(&bin).unwrap(), flags);
    }
}
