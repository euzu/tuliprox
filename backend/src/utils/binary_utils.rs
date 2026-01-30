use std::io;
use log::error;
use shared::error::to_io_error;

#[inline]
pub fn binary_serialize<T>(value: &T) -> io::Result<Vec<u8>>
where
    T: ?Sized + serde::Serialize,
{
    rmp_serde::to_vec(value).map_err(to_io_error)
}

#[inline]
pub fn binary_deserialize<T>(value: &[u8]) -> io::Result<T>
where
    T: for<'a> serde::Deserialize<'a>,
{
    rmp_serde::from_slice(value).map_err(|e| {
        error!("Failed to decode {e}");
        to_io_error(e)
    })
}

#[inline]
pub fn binary_serialize_into<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: io::Write,
    T: ?Sized + serde::Serialize,
{
    rmp_serde::encode::write(writer, value).map_err(to_io_error)
}
