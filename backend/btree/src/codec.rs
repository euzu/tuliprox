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
