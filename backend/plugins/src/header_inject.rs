//! Session-edge header injection (plan §7 *Session-edge header injection*,
//! §9 *Session-edge containment*): `inject-upstream-headers` under a 50 ms
//! clamp, a TTL cache per (plugin, user, channel), a header denylist,
//! fail-open.
//!
//! The stream proxy asks once per stream-session setup, never per chunk or
//! segment. A fresh cache hit never touches wasm. The whole step (every
//! plugin) sits under one `tokio::time::timeout`: `call_with_deadline` adds a
//! wall-clock slack of a second, so it cannot be the clamp, and the epoch
//! deadline of [`HookDispatcher::open_with`] only bounds CPU. A plugin that
//! traps, errs or does not answer in time adds nothing and is counted on its
//! circuit breaker; only successes are cached.

use crate::{
    dispatch::HookDispatcher,
    host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError},
    PluginSystem,
};
use http::{HeaderName, HeaderValue};
use shared::model::{PluginHeaderInjector, PluginHook, PluginUpstreamRequest};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, PoisonError,
    },
    time::Duration,
};
use tuliprox_core::model::PluginsConfig;
use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::stream_hooks::UpstreamRequest;

/// The hard clamp of the whole injection step (plan §9).
const HEADER_CLAMP: Duration = Duration::from_millis(50);

/// Cached (plugin, user, channel) entries at most. A full cache evicts the
/// entry that expires first, so churn never grows it.
const MAX_CACHED_ENTRIES: usize = 4096;

/// What one plugin's answer may carry: headers kept (the excess is dropped), and
/// the length of one name and one value.
const MAX_INJECTED_HEADERS: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 256;
const MAX_HEADER_VALUE_BYTES: usize = 4096;

/// Names the host never lets a plugin set (case-insensitive), besides `Proxy-*`:
/// they change message framing or the connection, not the request's identity.
const DENIED_HEADERS: [&str; 8] =
    ["host", "content-length", "transfer-encoding", "connection", "keep-alive", "te", "trailer", "upgrade"];

/// `true` for a name on the denylist.
fn is_denied(name: &str) -> bool {
    DENIED_HEADERS.iter().any(|denied| name.eq_ignore_ascii_case(denied))
        || name.as_bytes().get(..6).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"proxy-"))
}

/// Why a plugin-supplied header is dropped, `None` when it may be sent.
fn rejection(name: &str, value: &str) -> Option<&'static str> {
    if is_denied(name) {
        Some("on the denylist")
    } else if name.len() > MAX_HEADER_NAME_BYTES {
        Some("header name exceeds limit")
    } else if value.len() > MAX_HEADER_VALUE_BYTES {
        Some("header value exceeds limit")
    } else if HeaderName::from_bytes(name.as_bytes()).is_err() {
        Some("not a valid header name")
    } else if HeaderValue::from_str(value).is_err() {
        Some("not a valid header value")
    } else {
        None
    }
}

/// The headers of one plugin's answer that may be sent. A dropped header is
/// logged by name only: values can be credentials.
fn screen(plugin: &str, headers: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut screened = Vec::with_capacity(headers.len().min(MAX_INJECTED_HEADERS));
    let mut dropped_excess = 0;
    for (name, value) in headers {
        match rejection(&name, &value) {
            Some(reason) => {
                log::debug!("[plugin:{plugin}] dropped injected header `{name}`: {reason}");
            }
            None => {
                if screened.len() < MAX_INJECTED_HEADERS {
                    screened.push((name, value));
                } else {
                    dropped_excess += 1;
                }
            }
        }
    }
    if dropped_excess > 0 {
        log::warn!(
            "[plugin:{plugin}] dropped {dropped_excess} excess injected headers beyond cap of {MAX_INJECTED_HEADERS}"
        );
    }
    screened
}

/// Set `extra` on `headers`: a name already present (case-insensitive) is
/// replaced, so a later plugin overrides an earlier one.
fn merge_headers(headers: &mut Vec<(String, String)>, extra: &[(String, String)]) {
    for (name, value) in extra {
        match headers.iter_mut().find(|(present, _)| present.eq_ignore_ascii_case(name)) {
            Some(slot) => *slot = (name.clone(), value.clone()),
            None => headers.push((name.clone(), value.clone())),
        }
    }
}

/// What one injected answer is cached under. Each part is its own field, so
/// `("ab", "c")` and `("a", "bc")` never collide.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    plugin: Arc<str>,
    user: String,
    target_id: u16,
    virtual_id: u32,
}

struct CacheEntry {
    headers: Arc<[(String, String)]>,
    /// Host clock, milliseconds.
    expires_ms: u64,
}

/// Injected headers by (plugin, user, channel). Built once with the
/// [`crate::PluginSystem`].
pub(crate) struct HeaderCache {
    /// `plugins.stream_header_ttl`, in milliseconds. `0` caches nothing.
    ttl_ms: AtomicU64,
    entries: Mutex<HashMap<CacheKey, CacheEntry>>,
}

impl HeaderCache {
    pub(crate) fn new(config: &PluginsConfig) -> Self {
        Self { ttl_ms: AtomicU64::new(config.stream_header_ttl.get()), entries: Mutex::new(HashMap::new()) }
    }

    /// Follow a changed `plugins.stream_header_ttl` (config hot reload).
    #[allow(dead_code, reason = "the orchestrator calls it from `PluginSystem::reload`")]
    pub(crate) fn set_ttl(&self, config: &PluginsConfig) {
        self.ttl_ms.store(config.stream_header_ttl.get(), Ordering::Relaxed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<CacheKey, CacheEntry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The cached answer if it is still fresh at `now_ms`; an expired one is removed.
    fn get(&self, key: &CacheKey, now_ms: u64) -> Option<Arc<[(String, String)]>> {
        let mut entries = self.lock();
        match entries.get(key) {
            Some(entry) if entry.expires_ms > now_ms => Some(Arc::clone(&entry.headers)),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn insert(&self, key: CacheKey, headers: Arc<[(String, String)]>, now_ms: u64) {
        let ttl = self.ttl_ms.load(Ordering::Relaxed);
        if ttl == 0 {
            return;
        }
        let mut entries = self.lock();
        if entries.len() >= MAX_CACHED_ENTRIES && !entries.contains_key(&key) {
            entries.retain(|_, entry| entry.expires_ms > now_ms);
            if entries.len() >= MAX_CACHED_ENTRIES {
                let soonest = entries.iter().min_by_key(|(_, entry)| entry.expires_ms).map(|(key, _)| key.clone());
                if let Some(soonest) = soonest {
                    entries.remove(&soonest);
                }
            }
        }
        entries.insert(key, CacheEntry { headers, expires_ms: now_ms.saturating_add(ttl) });
    }

    #[cfg(test)]
    fn len(&self) -> usize { self.lock().len() }
}

#[allow(clippy::needless_pass_by_value, reason = "used as a `map_err` function")]
fn bind(err: wasmtime::Error) -> CallError { CallError::Trap(format!("{err:#}")) }

/// One plugin's `inject-upstream-headers` in a fresh instance, under the clamp.
async fn call_plugin(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    request: &PluginUpstreamRequest<'_>,
) -> Result<Vec<(String, String)>, CallError> {
    let indices = loaded
        .stream_hooks
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export stream-hooks".to_string()))?;
    let mut live = dispatcher.open_with(loaded, HEADER_CLAMP).await?;
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(bind)?;
    let wit = UpstreamRequest {
        virtual_id: request.virtual_id,
        url: request.url.to_string(),
        headers: request.headers.to_vec(),
        user: request.user.to_string(),
    };
    let limits = live.limits();
    let result = call_with_deadline(limits, guest.call_inject_upstream_headers(live.armed(), &wit)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    result?.map_err(|err| CallError::Trap(format!("hook returned err: {err}")))
}

/// Every injecting plugin in run order. `in_flight` names the plugin whose
/// wasm call is running, so the caller can count it when the clamp cuts the
/// step off.
async fn inject_all<'a>(
    system: &PluginSystem,
    dispatcher: &'a HookDispatcher<'_>,
    request: &PluginUpstreamRequest<'_>,
    headers: &mut Vec<(String, String)>,
    in_flight: &mut Option<&'a Arc<PluginRecord>>,
) {
    let cache = system.headers();
    for (plugin, loaded) in dispatcher.runnable(PluginHook::InjectHeaders) {
        if !loaded.env.capabilities.stream_headers {
            continue;
        }
        let key = CacheKey {
            plugin: Arc::clone(&plugin.id),
            user: request.user.to_string(),
            target_id: request.target_id,
            virtual_id: request.virtual_id,
        };
        let cached = cache.get(&key, system.shared().clock.wall_ms().get());
        let injected = if let Some(hit) = cached {
            hit
        } else {
            *in_flight = Some(plugin);
            let outcome = call_plugin(dispatcher, loaded, request).await;
            *in_flight = None;
            match outcome {
                Ok(raw) => {
                    plugin.report(Ok(()));
                    let screened: Arc<[(String, String)]> = screen(&plugin.id, raw).into();
                    cache.insert(key, Arc::clone(&screened), system.shared().clock.wall_ms().get());
                    screened
                }
                Err(err) => {
                    plugin.report(Err(&err));
                    log::warn!("[plugin:{}] inject-upstream-headers failed, no headers added: {err}", plugin.id);
                    continue;
                }
            }
        };
        merge_headers(headers, &injected);
    }
}

impl PluginHeaderInjector for PluginSystem {
    fn wants_upstream_headers(&self) -> bool {
        self.dispatcher().runnable(PluginHook::InjectHeaders).any(|(_, loaded)| loaded.env.capabilities.stream_headers)
    }

    async fn inject_upstream_headers(&self, request: &PluginUpstreamRequest<'_>) -> Vec<(String, String)> {
        let dispatcher = self.dispatcher();
        let mut headers = Vec::new();
        let mut in_flight = None;
        let step = inject_all(self, &dispatcher, request, &mut headers, &mut in_flight);
        if tokio::time::timeout(HEADER_CLAMP, step).await.is_err() {
            if let Some(plugin) = in_flight {
                plugin.report(Err(&CallError::Deadline));
                log::warn!(
                    "[plugin:{}] inject-upstream-headers exceeded {} ms, no headers added",
                    plugin.id,
                    HEADER_CLAMP.as_millis()
                );
            }
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_core::utils::{Clock, ManualClock};

    fn key(plugin: &str, user: &str, target_id: u16, virtual_id: u32) -> CacheKey {
        CacheKey { plugin: Arc::from(plugin), user: user.to_string(), target_id, virtual_id }
    }

    fn answer(value: &str) -> Arc<[(String, String)]> { vec![("X-Edge".to_string(), value.to_string())].into() }

    fn cache_with_ttl(ttl_ms: u64) -> HeaderCache {
        let config =
            PluginsConfig { stream_header_ttl: shared::model::Millis::new(ttl_ms), ..PluginsConfig::default() };
        HeaderCache::new(&config)
    }

    #[test]
    fn denylist_drops_framing_and_connection_headers_in_any_case() {
        for name in [
            "Host",
            "host",
            "CONTENT-LENGTH",
            "Transfer-Encoding",
            "Connection",
            "Keep-Alive",
            "Proxy-Authorization",
            "proxy-connection",
            "PROXY-X",
            "TE",
            "Trailer",
            "Upgrade",
        ] {
            assert!(is_denied(name), "{name} must be denied");
        }
        for name in ["X-Edge-Probe", "Authorization", "Cookie", "Referer", "Proxy", "Tex", "Hosts", "Pro"] {
            assert!(!is_denied(name), "{name} must pass");
        }
    }

    #[test]
    fn screen_drops_denied_and_unparsable_headers_and_keeps_the_rest() {
        let screened = screen(
            "p",
            vec![
                ("X-Edge-Probe".to_string(), "1".to_string()),
                ("Host".to_string(), "evil".to_string()),
                ("bad name".to_string(), "v".to_string()),
                ("X-Bad-Value".to_string(), "line\r\nbreak".to_string()),
                (String::new(), "v".to_string()),
                ("Authorization".to_string(), "Bearer t".to_string()),
            ],
        );
        assert_eq!(
            screened,
            vec![("X-Edge-Probe".to_string(), "1".to_string()), ("Authorization".to_string(), "Bearer t".to_string())]
        );
    }

    #[test]
    fn screen_caps_header_count_name_length_and_value_length() {
        let long_name = format!("X-{}", "a".repeat(255)); // 257 bytes > 256
        let long_val = "v".repeat(4097); // 4097 bytes > 4 KiB
        let mut headers = vec![(long_name, "ok".to_string()), ("X-Long-Val".to_string(), long_val)];
        for i in 0..40 {
            headers.push((format!("X-Hdr-{i}"), format!("val-{i}")));
        }
        let screened = screen("p", headers);
        assert_eq!(screened.len(), 32);
        assert_eq!(screened[0], ("X-Hdr-0".to_string(), "val-0".to_string()));
        assert_eq!(screened[31], ("X-Hdr-31".to_string(), "val-31".to_string()));
    }

    #[test]
    fn merge_replaces_a_name_case_insensitively_and_appends_new_ones() {
        let mut headers = vec![("x-edge".to_string(), "a".to_string())];
        merge_headers(
            &mut headers,
            &[("X-Edge".to_string(), "b".to_string()), ("X-Other".to_string(), "c".to_string())],
        );
        assert_eq!(headers, vec![("X-Edge".to_string(), "b".to_string()), ("X-Other".to_string(), "c".to_string())]);
    }

    #[test]
    fn a_hit_is_served_until_the_ttl_passes() {
        let clock = ManualClock::new(1_000);
        let cache = cache_with_ttl(60_000);
        let key = key("edge", "alice", 1, 7);
        assert!(cache.get(&key, clock.now_ms().get()).is_none(), "nothing cached yet");
        cache.insert(key.clone(), answer("1"), clock.now_ms().get());

        clock.advance(59_999);
        assert_eq!(cache.get(&key, clock.now_ms().get()).as_deref(), Some(&*answer("1")), "still fresh");
        clock.advance(1);
        assert!(cache.get(&key, clock.now_ms().get()).is_none(), "expired exactly at the ttl");
        assert_eq!(cache.len(), 0, "the expired entry is removed");
    }

    #[test]
    fn a_zero_ttl_caches_nothing() {
        let cache = cache_with_ttl(0);
        cache.insert(key("edge", "alice", 1, 7), answer("1"), 1_000);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn set_ttl_applies_to_later_inserts() {
        let cache = cache_with_ttl(0);
        let config = PluginsConfig { stream_header_ttl: shared::model::Millis::new(5_000), ..PluginsConfig::default() };
        cache.set_ttl(&config);
        cache.insert(key("edge", "alice", 1, 7), answer("1"), 1_000);
        assert!(cache.get(&key("edge", "alice", 1, 7), 5_999).is_some());
        assert!(cache.get(&key("edge", "alice", 1, 7), 6_000).is_none());
    }

    #[test]
    fn every_part_of_the_key_namespaces_the_entry() {
        let cache = cache_with_ttl(60_000);
        cache.insert(key("edge", "alice", 1, 7), answer("1"), 0);
        assert!(cache.get(&key("edge", "alice", 1, 7), 1).is_some());
        for other in [
            key("other", "alice", 1, 7),
            key("edge", "bob", 1, 7),
            key("edge", "alice", 2, 7),
            key("edge", "alice", 1, 8),
            key("edg", "ealice", 1, 7),
        ] {
            assert!(cache.get(&other, 1).is_none(), "{other:?} must not share an entry");
        }
    }

    #[test]
    fn a_full_cache_evicts_expired_entries_first_and_never_grows() {
        let cache = cache_with_ttl(1_000);
        for index in 0..MAX_CACHED_ENTRIES {
            cache.insert(key("edge", "alice", 1, u32::try_from(index).unwrap_or(u32::MAX)), answer("1"), 0);
        }
        assert_eq!(cache.len(), MAX_CACHED_ENTRIES);
        // Every entry has expired at t = 5 s, so the sweep empties it.
        cache.insert(key("edge", "bob", 1, 1), answer("2"), 5_000);
        assert_eq!(cache.len(), 1);

        // Nothing expired: the cap holds by evicting the soonest to expire.
        let cache = cache_with_ttl(60_000);
        for index in 0..MAX_CACHED_ENTRIES {
            let at = u64::try_from(index).unwrap_or(0);
            cache.insert(key("edge", "alice", 1, u32::try_from(index).unwrap_or(u32::MAX)), answer("1"), at);
        }
        cache.insert(key("edge", "bob", 1, 1), answer("2"), 10_000);
        assert_eq!(cache.len(), MAX_CACHED_ENTRIES);
        assert!(cache.get(&key("edge", "alice", 1, 0), 10_000).is_none(), "the soonest entry went");
        assert!(cache.get(&key("edge", "bob", 1, 1), 10_000).is_some());
    }
}
