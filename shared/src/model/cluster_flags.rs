use std::fmt;
use bitflags::bitflags;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use serde::de::{Error, SeqAccess, Visitor};
use crate::model::{PlaylistItemType, XtreamCluster};

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
   pub struct ClusterFlags: u16 {
        const Live   = 1;      // 0b0000_0001
        const Vod    = 1 << 1; // 0b0000_0010
        const Series = 1 << 2; // 0b0000_0100
    }
}

impl ClusterFlags {
    pub fn has_cluster(&self, item_type: PlaylistItemType) -> bool {
        XtreamCluster::try_from(item_type).ok().is_some_and(|cluster| match cluster {
            XtreamCluster::Live => self.contains(ClusterFlags::Live),
            XtreamCluster::Video => self.contains(ClusterFlags::Vod),
            XtreamCluster::Series => self.contains(ClusterFlags::Series),
        })
    }

    pub fn has_full_flags(&self) -> bool {
        self.is_all()
    }

    fn from_items<I, S>(items: I) -> Result<Self, &'static str>
    where
        I: IntoIterator<Item=S>,
        S: AsRef<str>,
    {
        let mut result = ClusterFlags::empty();

        for item in items {
            match item.as_ref().trim() {
                "live" => result.set(ClusterFlags::Live, true),
                "vod" => result.set(ClusterFlags::Vod, true),
                "series" => result.set(ClusterFlags::Series, true),
                _ => return Err("Invalid flag {item}, allowed are live, vod, series"),
            }
        }

        Ok(result)
    }
}

impl fmt::Display for ClusterFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut flag_strings = Vec::new();
        if self.contains(ClusterFlags::Live) {
            flag_strings.push("live");
        }
        if self.contains(ClusterFlags::Vod) {
            flag_strings.push("vod");
        }
        if self.contains(ClusterFlags::Series) {
            flag_strings.push("series");
        }

        write!(f, "[{}]", flag_strings.join(","))
    }
}

impl TryFrom<&str> for ClusterFlags {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let input = value.trim().trim_matches(['[', ']'].as_ref());
        let items = input.split(',').map(str::trim);
        ClusterFlags::from_items(items)
    }
}

impl TryFrom<Vec<String>> for ClusterFlags {
    type Error = &'static str;

    fn try_from(value: Vec<String>) -> Result<Self, Self::Error> {
        ClusterFlags::from_items(value)
    }
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
                let mut values = Vec::new();
                while let Some(val) = seq.next_element::<String>()? {
                    let entry = val.trim().to_lowercase();
                    values.push(entry);
                }
                ClusterFlags::try_from(values).map_err(A::Error::custom)
            }
        }
        deserializer.deserialize_any(ClusterFlagsVisitor)
    }
}
