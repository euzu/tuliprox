//! The `host` interface a guest imports: log, kv, get-config, now-unix,
//! cache-image and run-process.

use crate::{
    kv::{KvHandle, KvWrites},
    logs::PluginLog,
    process,
    state::HostState,
};
use std::sync::Arc;
use tuliprox_core::utils::Clock;
use tuliprox_plugin_api::bindings::tuliprox::plugin::{host, host_v1_1, types};

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

/// The host-fn side of one `Store`: the KV writes of the call in flight, which
/// only that call sees until [`end_call`] applies them. (The log rate limiter
/// is per plugin, not per store, so it lives in [`PluginLog`].)
pub(crate) struct StoreHostState {
    kv_writes: KvWrites,
    /// Values the call in flight must never write into a log line: the resolved
    /// secrets of an `input_scoped` rpc call, in addition to the plugin's own.
    call_secrets: Vec<String>,
}

impl StoreHostState {
    pub(crate) fn new() -> Self { Self { kv_writes: KvWrites::default(), call_secrets: Vec::new() } }

    /// Redact `secrets` from every log line of the call in flight.
    pub(crate) fn set_call_secrets(&mut self, secrets: Vec<String>) { self.call_secrets = secrets; }

    /// The plugin's own secrets and the call's, concatenated only when the call
    /// brought its own (the usual case allocates nothing).
    fn secrets<'a>(&'a self, env: &'a crate::state::PluginEnv) -> std::borrow::Cow<'a, [String]> {
        if self.call_secrets.is_empty() {
            std::borrow::Cow::Borrowed(env.secrets.as_ref())
        } else {
            std::borrow::Cow::Owned(env.secrets.iter().chain(self.call_secrets.iter()).cloned().collect())
        }
    }
}

/// Called by the host after every guest call returns (or traps): applies and
/// commits the call's buffered KV writes, so the writes of a call that trapped
/// are kept too. Never fails the call; an error is logged.
pub(crate) async fn end_call(state: &mut HostState) {
    if state.host.kv_writes.is_empty() {
        return;
    }
    let writes = state.host.kv_writes.take();
    let env = &state.env;
    let Some(kv) = &env.host.kv else { return };
    let message = match kv.apply(writes).await {
        Ok(0) => return,
        Ok(skipped) => format!("{skipped} plugin KV writes were dropped: the quota is exhausted"),
        Err(err) => format!("committing the plugin KV writes failed: {err:?}"),
    };
    let secrets = state.host.secrets(env);
    env.host.log.record(&env.id, &secrets, env.clock.wall_ms().get(), log::Level::Warn, &message);
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
        let secrets = self.host.secrets(env);
        env.host.log.record(&env.id, &secrets, env.clock.wall_ms().get(), log_level(level), &msg);
    }

    async fn kv_get(&mut self, key: String) -> Result<Option<Vec<u8>>, host::KvError> {
        let Some(kv) = &self.env.host.kv else { return Err(kv_unavailable()) };
        self.host.kv_writes.get(kv, key).await
    }

    async fn kv_set(&mut self, key: String, value: Vec<u8>) -> Result<(), host::KvError> {
        let Some(kv) = &self.env.host.kv else { return Err(kv_unavailable()) };
        self.host.kv_writes.set(kv, key, value).await
    }

    async fn kv_delete(&mut self, key: String) -> Result<(), host::KvError> {
        let Some(kv) = &self.env.host.kv else { return Err(kv_unavailable()) };
        self.host.kv_writes.delete(kv, key).await
    }

    async fn get_config(&mut self) -> String { self.env.settings.to_string() }

    async fn now_unix(&mut self) -> i64 { i64::try_from(self.env.clock.now_ms().get() / 1000).unwrap_or(i64::MAX) }

    async fn cache_image(&mut self, url: String) -> Result<String, String> {
        let env = Arc::clone(&self.env);
        env.images.cache_image(&env.http, &url).await
    }

    async fn run_process(
        &mut self,
        program: String,
        args: Vec<String>,
        env_vars: Vec<(String, String)>,
        stdin: Vec<u8>,
        timeout_secs: u32,
    ) -> Result<host::ProcessOutput, String> {
        let env = Arc::clone(&self.env);
        let request = process::Request { program, args, env: env_vars, stdin, timeout_secs };
        let output = process::run(&env.id, env.capabilities.process, request).await?;
        Ok(host::ProcessOutput { status: output.status, stdout: output.stdout, stderr: output.stderr })
    }
}

#[allow(clippy::unused_async_trait_impl)]
impl host_v1_1::Host for HostState {
    async fn cache_image_with_headers(
        &mut self,
        url: String,
        headers: Vec<(String, String)>,
    ) -> Result<String, String> {
        let env = Arc::clone(&self.env);
        env.images.cache_image_with_headers(&env.http, &url, &headers).await
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

    use crate::{
        manifest::Capabilities,
        state::{PluginEnv, PluginServices, Shared},
    };
    use host::Host;
    use std::path::Path;

    fn env(dir: &Path) -> Arc<PluginEnv> {
        let shared = Shared::new(PluginServices::new(reqwest::Client::new(), dir.to_path_buf()));
        let capabilities = Capabilities { storage: true, ..Capabilities::default() };
        shared.env_for(&Arc::from("p"), capabilities, Arc::from("{}"), Arc::from([])).expect("env")
    }

    #[tokio::test]
    async fn a_call_reads_its_own_buffered_writes_and_nobody_else_does_until_end_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env(dir.path());
        let (mut call, mut other) = (HostState::new(Arc::clone(&env), 1 << 20), HostState::new(env, 1 << 20));
        call.kv_set("k".to_string(), b"v".to_vec()).await.expect("set");
        assert_eq!(call.kv_get("k".to_string()).await.expect("get"), Some(b"v".to_vec()));
        assert_eq!(other.kv_get("k".to_string()).await.expect("get"), None);

        end_call(&mut call).await;
        assert_eq!(other.kv_get("k".to_string()).await.expect("get"), Some(b"v".to_vec()));
        // A delete hides the stored value from the call at once, from others after `end_call`.
        call.kv_delete("k".to_string()).await.expect("delete");
        assert_eq!(call.kv_get("k".to_string()).await.expect("get"), None);
        assert_eq!(other.kv_get("k".to_string()).await.expect("get"), Some(b"v".to_vec()));
        end_call(&mut call).await;
        assert_eq!(other.kv_get("k".to_string()).await.expect("get"), None);
    }

    /// The `log` ring of the plugin, as the management route hands it out.
    fn log_lines(env: &PluginEnv) -> Vec<String> {
        let query = crate::logs::LogQuery { min_level: None, since_ms: None, limit: None };
        env.host.log.entries(&query).into_iter().map(|line| line.message).collect()
    }

    #[tokio::test]
    async fn a_calls_secrets_are_redacted_from_the_guests_log_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env(dir.path());
        let mut call = HostState::new(Arc::clone(&env), 1 << 20);
        call.host.set_call_secrets(vec!["hunter2".to_string(), "alice".to_string()]);
        call.log(types::LogLevel::Info, "login alice with hunter2".to_string()).await;
        call.log(types::LogLevel::Info, "nothing secret here".to_string()).await;

        let lines = log_lines(&env);
        assert!(lines.iter().any(|line| line.contains("nothing secret here")), "an ordinary line is kept: {lines:?}");
        for line in &lines {
            assert!(!line.contains("hunter2") && !line.contains("alice"), "{line}");
        }

        // A fresh call without input secrets redacts only the plugin's own.
        let mut other = HostState::new(env, 1 << 20);
        other.log(types::LogLevel::Info, "hunter2 again".to_string()).await;
        assert!(log_lines(&other.env).iter().any(|line| line.contains("hunter2")), "no per-call secrets, no redaction");
    }

    #[tokio::test]
    async fn end_call_commits_only_the_ending_calls_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = env(dir.path());
        let (mut first, mut second) = (HostState::new(Arc::clone(&env), 1 << 20), HostState::new(env, 1 << 20));
        first.kv_set("first".to_string(), vec![1]).await.expect("set");
        second.kv_set("second".to_string(), vec![2]).await.expect("set");

        end_call(&mut first).await;
        let mut reader = HostState::new(Arc::clone(&first.env), 1 << 20);
        assert_eq!(reader.kv_get("first".to_string()).await.expect("get"), Some(vec![1]));
        assert_eq!(reader.kv_get("second".to_string()).await.expect("get"), None, "still running, not durable");

        end_call(&mut second).await;
        assert_eq!(reader.kv_get("second".to_string()).await.expect("get"), Some(vec![2]));
    }
}
