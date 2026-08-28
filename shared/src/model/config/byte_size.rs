use crate::utils::parse_size_base_2;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

/// A resolved byte count.
///
/// [`ByteSize`] is the *config-facing* form: the user's own spelling, kept as a
/// string so the web UI can show `10MB` back rather than `10485760`, and so
/// `From<&FfprobeConfig> for FfprobeConfigDto` can round-trip it. `Bytes` is
/// what that string resolves to once, at config load.
///
/// Keeping the two distinct means a runtime struct cannot accidentally hold an
/// unparsed size, and a parsed size cannot be mistaken for some other `u64`.
/// `#[repr(transparent)]` so this costs nothing over the `u64` it replaces.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub struct Bytes(pub u64);

impl Bytes {
    #[inline]
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[inline]
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Floors at one byte. Several probe sizes treat `0` as "unset" rather than
    /// "no bytes", and clamped individually at each call site.
    #[inline]
    #[must_use]
    pub const fn at_least_1(self) -> Self {
        Self(if self.0 == 0 { 1 } else { self.0 })
    }
}

impl std::fmt::Display for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteSize(String);

impl ByteSize {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn clean_or_default(&mut self, default_value: &str) {
        let trimmed = self.0.trim();
        self.0 = if trimmed.is_empty() { default_value.to_string() } else { trimmed.to_string() };
    }

    pub fn parse_bytes(&self) -> Result<Bytes, String> {
        parse_size_base_2(self.0.as_str()).map(Bytes::new)
    }
}

impl From<String> for ByteSize {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for ByteSize {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    use super::{ByteSize, Bytes};

    #[test]
    fn bytes_is_transparent_over_u64_and_floors_at_one() {
        assert_eq!(size_of::<Bytes>(), size_of::<u64>());
        assert_eq!(size_of::<Option<Bytes>>(), size_of::<Option<u64>>());
        assert_eq!(Bytes::new(0).at_least_1(), Bytes::new(1));
        assert_eq!(Bytes::new(9).at_least_1(), Bytes::new(9));
    }

    #[test]
    fn byte_size_keeps_the_users_spelling_while_bytes_carries_the_value() {
        // The string form is not redundant: the runtime -> Dto conversion sends it
        // back to the web UI so a user who wrote `10MB` sees `10MB`, not 10485760.
        let written = ByteSize::new("10MB");
        assert_eq!(written.as_str(), "10MB");
        assert_eq!(written.parse_bytes().expect("10MB parses"), Bytes::new(10_485_760));
    }

    #[test]
    fn parse_bytes_handles_binary_units() {
        // parse_bytes is binary (parse_size_base_2): 10GB = 10 * 1024^3, 512MB = 512 * 1024^2.
        assert_eq!(ByteSize::new("10GB").parse_bytes().expect("10GB should parse"), Bytes::new(10_737_418_240));
        assert_eq!(ByteSize::new("512MB").parse_bytes().expect("512MB should parse"), Bytes::new(536_870_912));
        assert_eq!(ByteSize::new("1048576").parse_bytes().expect("bytes should parse"), Bytes::new(1_048_576));
        assert_eq!(ByteSize::new("1GiB").parse_bytes().expect("1GiB should parse"), Bytes::new(1_073_741_824));
        assert_eq!(ByteSize::new("0").parse_bytes().expect("0 should parse"), Bytes::new(0));
    }
}
