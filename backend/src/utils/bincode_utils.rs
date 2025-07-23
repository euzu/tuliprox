use std::io;
use log::error;
use shared::error::to_io_error;

#[inline]
pub fn bincode_serialize<T>(value: &T) -> io::Result<Vec<u8>>
where
    T: ?Sized + serde::Serialize,
{
    bincode::serde::encode_to_vec(value, bincode::config::legacy()).map_err(to_io_error)
}

#[inline]
pub fn bincode_deserialize<T>(value: &[u8]) -> io::Result<T>
where
    T: for<'a> serde::Deserialize<'a>,
{
  match bincode::serde::decode_from_slice(value, bincode::config::legacy()) {
     Ok((instance, _size)) => Ok(instance),
      Err(e) => {
          error!("Failed to decode {e}");
          Err(to_io_error(e))
      },
  }
}
