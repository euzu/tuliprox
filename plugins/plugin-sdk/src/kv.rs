//! The plugin's key/value store (`host::kv-*`): raw bytes, serde JSON, and TTL
//! keys.
//!
//! The host bounds it: a key is at most 1 KiB, a value at most 1 MiB, the store
//! 50 MiB per plugin. KV writes change what later hooks return, so they feed
//! the plugin fingerprint (plan §7 *Determinism*).
//!
//! # TTL
//!
//! The store has no expiry. [`set_ttl`] prefixes the value with its expiry,
//! `host::now-unix() + ttl`, and [`get_fresh`] drops (and deletes) a value whose
//! expiry has passed. Values written with [`set_ttl`] must be read with
//! [`get_fresh`] (and the JSON twins); the plain readers return the prefix too.

use crate::{bindings::tuliprox::plugin::host::KvError, clock, sys};
use serde::{de::DeserializeOwned, Serialize};
use std::fmt;

/// Why a KV call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    QuotaExceeded,
    KeyTooLarge,
    ValueTooLarge,
    /// The host store is unavailable (disabled, I/O error).
    Unavailable(String),
    /// A value was not valid UTF-8 / JSON / a TTL envelope.
    Codec(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuotaExceeded => f.write_str("kv quota exceeded"),
            Self::KeyTooLarge => f.write_str("kv key too large"),
            Self::ValueTooLarge => f.write_str("kv value too large"),
            Self::Unavailable(msg) => write!(f, "kv unavailable: {msg}"),
            Self::Codec(msg) => write!(f, "kv value: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<KvError> for Error {
    fn from(err: KvError) -> Self {
        match err {
            KvError::QuotaExceeded => Self::QuotaExceeded,
            KvError::KeyTooLarge => Self::KeyTooLarge,
            KvError::ValueTooLarge => Self::ValueTooLarge,
            KvError::Unavailable(msg) => Self::Unavailable(msg),
        }
    }
}

/// So a hook can `?` a KV call straight into its `Result<_, String>`.
impl From<Error> for String {
    fn from(err: Error) -> Self { err.to_string() }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The raw value of `key`.
pub fn get(key: &str) -> Result<Option<Vec<u8>>> { Ok(sys::kv_get(key)?) }

/// Stores `value` under `key`.
pub fn set(key: &str, value: &[u8]) -> Result<()> { Ok(sys::kv_set(key, value)?) }

/// Removes `key` (not an error when absent).
pub fn delete(key: &str) -> Result<()> { Ok(sys::kv_delete(key)?) }

/// The value of `key` as UTF-8 text.
pub fn get_str(key: &str) -> Result<Option<String>> {
    get(key)?.map(|bytes| String::from_utf8(bytes).map_err(|err| Error::Codec(err.to_string()))).transpose()
}

/// Stores `value` as UTF-8 text.
pub fn set_str(key: &str, value: &str) -> Result<()> { set(key, value.as_bytes()) }

/// The value of `key`, parsed as JSON.
pub fn get_json<T: DeserializeOwned>(key: &str) -> Result<Option<T>> {
    get(key)?.map(|bytes| serde_json::from_slice(&bytes).map_err(|err| Error::Codec(err.to_string()))).transpose()
}

/// Stores `value` as JSON.
pub fn set_json<T: Serialize + ?Sized>(key: &str, value: &T) -> Result<()> { set(key, &encode_json(value)?) }

fn encode_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|err| Error::Codec(err.to_string()))
}

const EXPIRY_LEN: usize = 8;

/// Stores `value` for `ttl_secs` seconds of host time: the value is prefixed
/// with its expiry, `now-unix() + ttl_secs` (big-endian `i64`).
pub fn set_ttl(key: &str, value: &[u8], ttl_secs: u32) -> Result<()> {
    let expires_at = clock::now_unix().saturating_add(i64::from(ttl_secs));
    let mut envelope = Vec::with_capacity(EXPIRY_LEN + value.len());
    envelope.extend_from_slice(&expires_at.to_be_bytes());
    envelope.extend_from_slice(value);
    set(key, &envelope)
}

/// The value written by [`set_ttl`] if it has not expired. An expired value is
/// deleted and reported as absent.
pub fn get_fresh(key: &str) -> Result<Option<Vec<u8>>> {
    let Some(envelope) = get(key)? else { return Ok(None) };
    let Some((head, value)) = envelope.split_at_checked(EXPIRY_LEN) else {
        return Err(Error::Codec(format!("`{key}` is not a TTL value")));
    };
    let expires_at = i64::from_be_bytes(head.try_into().map_err(|_| Error::Codec("short expiry".into()))?);
    if clock::now_unix() >= expires_at {
        delete(key)?;
        return Ok(None);
    }
    Ok(Some(value.to_vec()))
}

/// [`set_ttl`] with a JSON value.
pub fn set_json_ttl<T: Serialize + ?Sized>(key: &str, value: &T, ttl_secs: u32) -> Result<()> {
    set_ttl(key, &encode_json(value)?, ttl_secs)
}

/// [`get_fresh`] parsed as JSON.
pub fn get_json_fresh<T: DeserializeOwned>(key: &str) -> Result<Option<T>> {
    get_fresh(key)?.map(|bytes| serde_json::from_slice(&bytes).map_err(|err| Error::Codec(err.to_string()))).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock;

    #[test]
    fn raw_text_and_json_round_trip() {
        mock::reset();
        set("raw", b"\x00\xff").unwrap();
        assert_eq!(get("raw").unwrap(), Some(vec![0, 255]));
        set_str("text", "héllo").unwrap();
        assert_eq!(get_str("text").unwrap().as_deref(), Some("héllo"));
        set_json("json", &vec![1, 2, 3]).unwrap();
        assert_eq!(get_json::<Vec<i32>>("json").unwrap(), Some(vec![1, 2, 3]));
        delete("json").unwrap();
        assert_eq!(get_json::<Vec<i32>>("json").unwrap(), None);
        assert!(matches!(get_json::<u8>("text"), Err(Error::Codec(_))));
    }

    #[test]
    fn a_ttl_value_expires_on_the_host_clock() {
        mock::reset();
        mock::set_now(1_000);
        set_json_ttl("token", &"abc", 60).unwrap();
        assert_eq!(get_json_fresh::<String>("token").unwrap().as_deref(), Some("abc"));
        mock::advance(59);
        assert_eq!(get_json_fresh::<String>("token").unwrap().as_deref(), Some("abc"));
        mock::advance(1);
        assert_eq!(get_json_fresh::<String>("token").unwrap(), None);
        assert_eq!(get("token").unwrap(), None, "an expired value is deleted");
    }

    #[test]
    fn the_host_limits_surface_as_errors() {
        mock::reset();
        assert_eq!(set(&"k".repeat(1025), b"v"), Err(Error::KeyTooLarge));
        assert_eq!(set("k", &vec![0; (1 << 20) + 1]), Err(Error::ValueTooLarge));
        mock::set_kv_unavailable("disabled");
        assert_eq!(get("k"), Err(Error::Unavailable("disabled".into())));
        let as_hook_error: std::result::Result<(), String> = (|| Ok(set("k", b"v")?))();
        assert_eq!(as_hook_error, Err("kv unavailable: disabled".into()));
    }
}
