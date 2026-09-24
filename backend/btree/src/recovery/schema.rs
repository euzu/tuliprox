//! The application-supplied description of what a recovery record means.
//!
//! The engine never interprets a key or a value. It only knows how to ask the
//! application to encode one as field-named JSON, to step a historical record
//! forward one version at a time, and to decode a record that has reached the
//! current version. That indirection is the whole reason a V1 record can be
//! restored into a database whose current type is V3.

use serde_json::Value;
use std::io;

/// The maximum number of version steps the engine will take before it decides
/// that a schema's migration chain does not terminate.
pub(crate) const MAX_MIGRATION_STEPS: u32 = 1024;

/// Describes one recoverable key/value domain.
///
/// Implementations live in the application. Nothing in this trait may name a
/// type from this crate beyond `serde_json::Value`.
pub trait RecoverySchema<K, V> {
    /// Stable identifier written into every record. Two schemas with different
    /// names never share a recovery directory.
    const NAME: &'static str;
    /// The version that `decode_current_key` and `decode_current` understand.
    const CURRENT_VERSION: u32;

    fn encode_key(&self, key: &K) -> io::Result<Value>;

    /// Transforms a key from `from` to `from + 1`.
    fn migrate_key_one(&self, from: u32, key: Value) -> io::Result<Value>;

    fn decode_current_key(&self, key: Value) -> io::Result<K>;

    fn encode_current(&self, value: &V) -> io::Result<Value>;

    /// Transforms a value from `from` to `from + 1`.
    fn migrate_one(&self, from: u32, value: Value) -> io::Result<Value>;

    fn decode_current(&self, value: Value) -> io::Result<V>;
}

fn migration_error(message: String) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

/// Steps `key` from `from` up to `S::CURRENT_VERSION` and decodes it.
///
/// A record is never decoded at a version other than the current one, which is
/// what stops an old record from being reinterpreted as the current type.
pub(crate) fn migrate_key_to_current<K, V, S>(schema: &S, from: u32, mut key: Value) -> io::Result<K>
where
    S: RecoverySchema<K, V>,
{
    let mut version = check_start_version::<K, V, S>(from)?;
    let mut steps = 0u32;
    while version < S::CURRENT_VERSION {
        key = schema.migrate_key_one(version, key)?;
        version = version.checked_add(1).ok_or_else(|| migration_error("key schema version overflow".to_owned()))?;
        steps = steps.checked_add(1).ok_or_else(|| migration_error("key migration step overflow".to_owned()))?;
        if steps > MAX_MIGRATION_STEPS {
            return Err(migration_error(format!("key migration for schema {} did not terminate", S::NAME)));
        }
    }
    schema.decode_current_key(key)
}

/// Steps `value` from `from` up to `S::CURRENT_VERSION` and decodes it.
pub(crate) fn migrate_value_to_current<K, V, S>(schema: &S, from: u32, mut value: Value) -> io::Result<V>
where
    S: RecoverySchema<K, V>,
{
    let mut version = check_start_version::<K, V, S>(from)?;
    let mut steps = 0u32;
    while version < S::CURRENT_VERSION {
        value = schema.migrate_one(version, value)?;
        version = version.checked_add(1).ok_or_else(|| migration_error("value schema version overflow".to_owned()))?;
        steps = steps.checked_add(1).ok_or_else(|| migration_error("value migration step overflow".to_owned()))?;
        if steps > MAX_MIGRATION_STEPS {
            return Err(migration_error(format!("value migration for schema {} did not terminate", S::NAME)));
        }
    }
    schema.decode_current(value)
}

fn check_start_version<K, V, S>(from: u32) -> io::Result<u32>
where
    S: RecoverySchema<K, V>,
{
    if from == 0 {
        return Err(migration_error(format!("schema {} version must be nonzero", S::NAME)));
    }
    if from > S::CURRENT_VERSION {
        return Err(migration_error(format!(
            "schema {} record version {from} is newer than the supported version {}",
            S::NAME,
            S::CURRENT_VERSION
        )));
    }
    Ok(from)
}

/// Binds a recovery generation to the exact schema that produced it.
///
/// The current version is deliberately excluded: a build that raises
/// `CURRENT_VERSION` must still recognise its own older databases, and does so
/// by migrating them rather than by rejecting them.
pub(crate) fn schema_fingerprint(name: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tuliprox-recovery-schema\0");
    hasher.update(name.as_bytes());
    *hasher.finalize().as_bytes()
}
