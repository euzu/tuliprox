//! `MessagePack` codec for persisted B+Tree keys and values.
//!
//! The engine owns its own codec so that the on-disk encoding is a property of
//! the storage engine rather than of whatever application happens to link it.
//! The encoding is byte-for-byte the encoding this database has always written:
//! `rmp_serde` in its default (compact, positional) form.

use log::error;
use std::io;

#[inline]
fn encode_error<E: std::error::Error>(error: E) -> io::Error { io::Error::other(error.to_string()) }

#[inline]
pub(crate) fn binary_serialize<T>(value: &T) -> io::Result<Vec<u8>>
where
    T: ?Sized + serde::Serialize,
{
    rmp_serde::to_vec(value).map_err(encode_error)
}

#[inline]
pub(crate) fn binary_deserialize<T>(value: &[u8]) -> io::Result<T>
where
    T: for<'a> serde::Deserialize<'a>,
{
    rmp_serde::from_slice(value).map_err(|error| {
        error!("Failed to decode {error}");
        encode_error(error)
    })
}

#[inline]
pub(crate) fn binary_serialize_into<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: io::Write,
    T: ?Sized + serde::Serialize,
{
    rmp_serde::encode::write(writer, value).map_err(encode_error)
}

#[cfg(test)]
mod tests {
    use super::{binary_deserialize, binary_serialize};
    use serde::{Deserialize, Serialize};

    /// Stand-in for the domain id newtypes (`VirtualId`, `ProviderId`) that are
    /// used as persisted B+Tree keys.
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[repr(transparent)]
    #[serde(transparent)]
    struct TransparentKey(u32);

    /// The property that lets a bare `u32` key become a newtype without a
    /// database migration: `#[serde(transparent)]` encodes exactly as the inner
    /// value, so every database already on disk still reads back.
    #[test]
    fn transparent_newtype_keys_encode_identically_to_the_bare_integer() {
        for raw in [0u32, 1, 42, 4_294_967_295] {
            let bare = binary_serialize(&raw).expect("u32 encodes");
            let wrapped = binary_serialize(&TransparentKey(raw)).expect("newtype encodes");
            assert_eq!(bare, wrapped, "encoding differs for {raw}");

            // Cross-reads both ways: bytes written before the newtype existed
            // decode into it, and bytes it writes decode as a plain u32.
            assert_eq!(
                binary_deserialize::<TransparentKey>(&bare).expect("old bytes decode into the newtype"),
                TransparentKey(raw)
            );
            assert_eq!(binary_deserialize::<u32>(&wrapped).expect("new bytes decode as u32"), raw);
        }
    }
}
