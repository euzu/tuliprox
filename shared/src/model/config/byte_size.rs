use crate::utils::parse_size_base_2;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteSize(String);

impl ByteSize {
    pub fn new(value: impl Into<String>) -> Self { Self(value.into()) }

    pub fn as_str(&self) -> &str { self.0.as_str() }

    pub fn clean_or_default(&mut self, default_value: &str) {
        let trimmed = self.0.trim();
        self.0 = if trimmed.is_empty() { default_value.to_string() } else { trimmed.to_string() };
    }

    pub fn parse_bytes(&self) -> Result<u64, String> { parse_size_base_2(self.0.as_str()) }
}

impl From<String> for ByteSize {
    fn from(value: String) -> Self { Self::new(value) }
}

impl From<&str> for ByteSize {
    fn from(value: &str) -> Self { Self::new(value) }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

impl Serialize for ByteSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ByteSizeVisitor;

        impl de::Visitor<'_> for ByteSizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a byte size string or unsigned integer")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSize(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSize(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(ByteSize(value.to_string()))
            }
        }

        deserializer.deserialize_any(ByteSizeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::ByteSize;

    #[test]
    fn parse_bytes_handles_binary_units() {
        // parse_bytes is binary (parse_size_base_2): 10GB = 10 * 1024^3, 512MB = 512 * 1024^2.
        assert_eq!(ByteSize::new("10GB").parse_bytes().expect("10GB should parse"), 10_737_418_240);
        assert_eq!(ByteSize::new("512MB").parse_bytes().expect("512MB should parse"), 536_870_912);
        assert_eq!(ByteSize::new("1048576").parse_bytes().expect("bytes should parse"), 1_048_576);
        assert_eq!(ByteSize::new("1GiB").parse_bytes().expect("1GiB should parse"), 1_073_741_824);
        assert_eq!(ByteSize::new("0").parse_bytes().expect("0 should parse"), 0);
    }
}
