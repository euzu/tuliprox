use crate::utils::parse_uuid_hex;
use hex::FromHex;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UUIDType(pub [u8; 32]);

#[allow(clippy::len_without_is_empty)]
impl UUIDType {
    pub const fn len(&self) -> usize {
        32usize
    }

    /// Converts the first 16 bytes of this `UUIDType` into a valid UUID v4 string.
    ///
    /// Note:
    /// - Only the first 16 bytes are used, because a standard UUID is 16 bytes.
    /// - The remaining 16 bytes of the 32-byte `UUIDType` are ignored in this operation.
    /// - This conversion is **not reversible**, calling `from_valid_uuid` on the resulting string
    ///   will not recover the original 32-byte `UUIDType`.
    pub fn to_valid_uuid(&self) -> String {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&self.0[0..16]);

        // Set UUID version (v4)
        bytes[6] = (bytes[6] & 0x0F) | 0x40;
        // Set UUID variant (10xxxxxx)
        bytes[8] = (bytes[8] & 0x3F) | 0x80;

        format!(
            "{}-{}-{}-{}-{}",
            hex::encode_upper(&bytes[0..4]),
            hex::encode_upper(&bytes[4..6]),
            hex::encode_upper(&bytes[6..8]),
            hex::encode_upper(&bytes[8..10]),
            hex::encode_upper(&bytes[10..16]),
        )
    }

    /// Creates a `UUIDType` from a UUID string; if parsing fails, hashes the input
    /// to derive a deterministic 32-byte value.
    ///
    /// Implementation details:
    /// - A standard UUID is 16 bytes.
    /// - The first 16 bytes of the resulting `UUIDType` are taken from the parsed UUID.
    /// - The remaining 16 bytes are filled by hashing the first 16 bytes using Blake3.
    /// - This ensures the resulting `UUIDType` is 32 bytes, but this operation is **not reversible**
    ///   to the original 32-byte `UUIDType` if the input was previously generated with `to_valid_uuid`.
    pub fn from_valid_uuid(uuid: &str) -> Self {
        let bytes = if let Some(parsed_uuid) = parse_uuid_hex(uuid) {
            let mut bytes = [0u8; 32];
            // The 16 UUID Bytes
            bytes[..16].copy_from_slice(&parsed_uuid);
            // The remaining 16 bytes = hash of the UUID
            let hash = blake3::hash(&parsed_uuid);
            bytes[16..].copy_from_slice(&hash.as_bytes()[..16]);
            bytes
        } else {
            // Fallback: hash the entire input string
            *blake3::hash(uuid.as_bytes()).as_bytes()
        };

        UUIDType(bytes)
    }
}

impl AsRef<[u8]> for UUIDType {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl FromStr for UUIDType {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = <[u8; 32]>::from_hex(s)?;
        Ok(UUIDType(bytes))
    }
}

impl std::fmt::Display for UUIDType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl Serialize for UUIDType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

struct UUIDTypeVisitor;

impl<'de> serde::de::Visitor<'de> for UUIDTypeVisitor {
    type Value = UUIDType;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a 32-byte array")
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if v.len() == 32 {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(v);
            Ok(UUIDType(bytes))
        } else {
            Err(E::invalid_length(v.len(), &self))
        }
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = seq
                .next_element()?
                .ok_or_else(|| serde::de::Error::invalid_length(i, &self))?;
        }
        Ok(UUIDType(bytes))
    }
}

impl<'de> Deserialize<'de> for UUIDType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_bytes(UUIDTypeVisitor)
    }
}