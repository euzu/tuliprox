use crate::model::Millis;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// HLS media timeout. AUTO is resolved against the shared session at each attempt.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HlsSegmentTimeout {
    #[default]
    Auto,
    Fixed(Millis),
}

impl fmt::Display for HlsSegmentTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("AUTO"),
            Self::Fixed(value) => value.get().fmt(f),
        }
    }
}

impl Serialize for HlsSegmentTimeout {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Fixed(value) => serializer.serialize_u64(value.get()),
        }
    }
}

impl<'de> Deserialize<'de> for HlsSegmentTimeout {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TimeoutVisitor;
        impl de::Visitor<'_> for TimeoutVisitor {
            type Value = HlsSegmentTimeout;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .write_str("origin_segment_timeout_ms: \"auto\", \"AUTO\", or a positive integer in milliseconds")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if matches!(value, "auto" | "AUTO") {
                    Ok(HlsSegmentTimeout::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(value), &self))
                }
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                if value > 0 {
                    Ok(HlsSegmentTimeout::Fixed(Millis::new(value)))
                } else {
                    Err(E::invalid_value(de::Unexpected::Unsigned(value), &self))
                }
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                u64::try_from(value)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(value), &self))
                    .and_then(|value| self.visit_u64(value))
            }
        }
        deserializer.deserialize_any(TimeoutVisitor)
    }
}
