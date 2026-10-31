//! The one place that touches the host imports: wasm imports on `wasm32`, the
//! [`mock`](crate::mock) host everywhere else.

#[cfg(target_arch = "wasm32")]
mod imp {
    use crate::bindings::tuliprox::plugin::{host, types::LogLevel};

    pub(crate) fn log(level: LogLevel, msg: &str) { host::log(level, msg); }
    pub(crate) fn kv_get(key: &str) -> Result<Option<Vec<u8>>, host::KvError> { host::kv_get(key) }
    pub(crate) fn kv_set(key: &str, value: &[u8]) -> Result<(), host::KvError> { host::kv_set(key, value) }
    pub(crate) fn kv_delete(key: &str) -> Result<(), host::KvError> { host::kv_delete(key) }
    pub(crate) fn get_config() -> String { host::get_config() }
    pub(crate) fn now_unix() -> i64 { host::now_unix() }
    pub(crate) fn cache_image(url: &str) -> Result<String, String> { host::cache_image(url) }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use crate::{
        bindings::tuliprox::plugin::{host, types::LogLevel},
        mock,
    };

    pub(crate) fn log(level: LogLevel, msg: &str) { mock::record_log(level, msg); }
    pub(crate) fn kv_get(key: &str) -> Result<Option<Vec<u8>>, host::KvError> { mock::kv_get(key) }
    pub(crate) fn kv_set(key: &str, value: &[u8]) -> Result<(), host::KvError> { mock::kv_set(key, value) }
    pub(crate) fn kv_delete(key: &str) -> Result<(), host::KvError> { mock::kv_delete(key) }
    pub(crate) fn get_config() -> String { mock::config() }
    pub(crate) fn now_unix() -> i64 { mock::now_unix() }
    pub(crate) fn cache_image(url: &str) -> Result<String, String> { mock::cache_image(url) }
}

pub(crate) use imp::*;
