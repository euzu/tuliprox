//! The `host` interface a guest imports: log, kv, get-config, now-unix and
//! cache-image (plan §8).

use crate::{kv::KvHandle, logs::PluginLog, state::HostState};
use std::sync::Arc;
use tuliprox_core::utils::Clock;
use tuliprox_plugin_api::bindings::tuliprox::plugin::{host, types};

/// The host-fn side of a [`crate::state::PluginEnv`]: what all instances of one
/// plugin share.
pub(crate) struct PluginHostEnv {
    pub(crate) log: Arc<PluginLog>,
    /// `None` without the `storage` capability.
    pub(crate) kv: Option<KvHandle>,
}

impl PluginHostEnv {
    pub(crate) fn new(log: Arc<PluginLog>, kv: Option<KvHandle>) -> Self { Self { log, kv } }
}

/// The host-fn side of one `Store`: whether the guest wrote to the KV store
/// since the last commit. (The log rate limiter is per plugin, not per store,
/// so it lives in [`PluginLog`].)
pub(crate) struct StoreHostState {
    wrote: bool,
}

impl StoreHostState {
    pub(crate) fn new() -> Self { Self { wrote: false } }
}

/// Called by the host after every guest call returns (or traps): commits the
/// call's KV writes. Never fails the call; a commit error is logged.
pub(crate) async fn end_call(state: &mut HostState) {
    if !std::mem::take(&mut state.host.wrote) {
        return;
    }
    let env = &state.env;
    let Some(kv) = &env.host.kv else { return };
    if let Err(err) = kv.commit().await {
        let message = format!("committing the plugin KV writes failed: {err:?}");
        env.host.log.record(&env.id, &env.secrets, env.clock.wall_ms().get(), log::Level::Warn, &message);
    }
}

fn log_level(level: types::LogLevel) -> log::Level {
    match level {
        types::LogLevel::Trace => log::Level::Trace,
        types::LogLevel::Debug => log::Level::Debug,
        types::LogLevel::Info => log::Level::Info,
        types::LogLevel::Warn => log::Level::Warn,
        types::LogLevel::Error => log::Level::Error,
    }
}

impl types::Host for HostState {}

#[allow(clippy::unused_async_trait_impl)]
impl host::Host for HostState {
    async fn log(&mut self, level: types::LogLevel, msg: String) {
        let env = &self.env;
        env.host.log.record(&env.id, &env.secrets, env.clock.wall_ms().get(), log_level(level), &msg);
    }

    async fn kv_get(&mut self, key: String) -> Result<Option<Vec<u8>>, host::KvError> {
        let Some(kv) = &self.env.host.kv else { return Err(kv_unavailable()) };
        kv.get(key).await
    }

    async fn kv_set(&mut self, key: String, value: Vec<u8>) -> Result<(), host::KvError> {
        let Some(kv) = &self.env.host.kv else { return Err(kv_unavailable()) };
        kv.set(key, value).await?;
        self.host.wrote = true;
        Ok(())
    }

    async fn kv_delete(&mut self, key: String) -> Result<(), host::KvError> {
        let Some(kv) = &self.env.host.kv else { return Err(kv_unavailable()) };
        kv.delete(key).await?;
        self.host.wrote = true;
        Ok(())
    }

    async fn get_config(&mut self) -> String { self.env.settings.to_string() }

    async fn now_unix(&mut self) -> i64 { i64::try_from(self.env.clock.now_ms().get() / 1000).unwrap_or(i64::MAX) }

    async fn cache_image(&mut self, url: String) -> Result<String, String> {
        let env = Arc::clone(&self.env);
        env.images.cache_image(&env.http, &url).await
    }
}

fn kv_unavailable() -> host::KvError {
    host::KvError::Unavailable("the plugin has no storage capability, so it has no KV store".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_levels_map_to_log_levels() {
        assert_eq!(log_level(types::LogLevel::Trace), log::Level::Trace);
        assert_eq!(log_level(types::LogLevel::Debug), log::Level::Debug);
        assert_eq!(log_level(types::LogLevel::Info), log::Level::Info);
        assert_eq!(log_level(types::LogLevel::Warn), log::Level::Warn);
        assert_eq!(log_level(types::LogLevel::Error), log::Level::Error);
    }

    #[test]
    fn a_new_store_has_nothing_to_commit() {
        assert!(!StoreHostState::new().wrote);
    }
}
