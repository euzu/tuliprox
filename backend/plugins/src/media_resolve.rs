//! Stream and image resolution for media-server provider plugins
//! (`media-provider-v1-1.resolve-stream`, `media-provider-v1-1.resolve-image`).
//!
//! Under:
//! - Manifest `resources.resolve_ms` deadline (clamped to [1 ms, 30 s], default 5 s)
//! - Output checks: URL must parse and have scheme http or https with a valid host
//! - Header checks: capped count (32), total bytes (16 KiB), and filtered against
//!   the host denylist (`header_inject::is_denied`)
//! - Network checks: plugin must have capability `network`
//! - Result cache: per (input, plugin, `item_id`, `stream_key` or `image_key`), capped
//!   at 1 hour, bounded size, concurrent coalescence, failed resolves not cached,
//!   and no lock held across `.await`.

use crate::{
    dispatch::{count, HookDispatcher},
    header_inject, host_fns,
    registry::{LoadedPlugin, PluginRecord},
    runtime::{call_with_deadline, CallError},
};
use http::{HeaderName, HeaderValue};
use lru::LruCache;
use shared::model::{
    HookCtx, PluginHook, PluginHookStats, PluginImageRequest, PluginRunReport, PluginStreamRequest, PluginStreamTarget,
};
use std::{
    collections::HashMap,
    future::Future,
    num::NonZeroUsize,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use tuliprox_plugin_api::bindings::exports::tuliprox::plugin::media_provider_v1_1::{
    ImageRequest as WitImageRequest, StreamRequest as WitStreamRequest, StreamTarget as WitStreamTarget,
};

/// Maximum cached entries in the resolution cache.
pub const MAX_RESOLVE_CACHE_ENTRIES: usize = 4096;

/// Maximum TTL allowed for cached resolution targets (1 hour).
pub const MAX_RESOLVE_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Maximum headers a stream target may carry.
pub const MAX_TARGET_HEADERS: usize = 32;

/// Maximum bytes for one header name.
pub const MAX_TARGET_HEADER_NAME_BYTES: usize = 256;

/// Maximum bytes for one header value.
pub const MAX_TARGET_HEADER_VALUE_BYTES: usize = 4096;

/// Maximum total bytes for all header names and values.
pub const MAX_TARGET_HEADER_TOTAL_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResolveTargetKey {
    Stream { stream_key: Option<String>, range: Option<String> },
    Image(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolveKey {
    pub input: String,
    pub plugin: Arc<str>,
    pub item_id: String,
    pub target: ResolveTargetKey,
}

struct CacheEntry {
    target: PluginStreamTarget,
    expires_at: Instant,
}

/// Cache of resolved stream and image targets with concurrent request coalescing.
pub struct ResolveCache {
    entries: Mutex<LruCache<ResolveKey, CacheEntry>>,
    in_flight: InFlightMap,
}

type ResolveOutcome = Option<Result<PluginStreamTarget, String>>;
type InFlightMap = Mutex<HashMap<ResolveKey, watch::Receiver<ResolveOutcome>>>;

enum Role {
    Leader(watch::Sender<ResolveOutcome>),
    Follower(watch::Receiver<ResolveOutcome>),
}

impl Default for ResolveCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(LruCache::new(NonZeroUsize::new(MAX_RESOLVE_CACHE_ENTRIES).unwrap())),
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

impl ResolveCache {
    pub fn new() -> Self { Self::default() }

    /// Clears all cached entries, leaving in-flight resolutions to finish.
    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.clear();
    }

    /// Retrieve a cached target or execute `resolve_fn` with concurrency coalescing.
    ///
    /// Never holds any lock across an `.await`.
    pub async fn get_or_resolve<F, Fut>(&self, key: ResolveKey, resolve_fn: F) -> Result<PluginStreamTarget, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<PluginStreamTarget, String>>,
    {
        // 1. Check cache under lock
        {
            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(entry) = entries.get(&key) {
                if Instant::now() < entry.expires_at {
                    return Ok(entry.target.clone());
                }
                entries.pop(&key);
            }
        }

        // 2. Check in-flight coalescence under lock
        let role = {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = in_flight.get(&key) {
                Role::Follower(existing.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                in_flight.insert(key.clone(), rx);
                Role::Leader(tx)
            }
        };

        match role {
            Role::Leader(tx) => {
                // Removes the in-flight entry on every exit, also when the caller
                // drops this future mid-resolve (client disconnect): a stale entry
                // would turn every later request for the key into a dead follower.
                let guard = InFlightGuard { in_flight: &self.in_flight, key: &key };
                let result = resolve_fn().await;
                drop(guard);

                if let Ok(ref target) = result {
                    if let Some(ttl_secs) = target.ttl_secs {
                        if ttl_secs > 0 {
                            let ttl = Duration::from_secs(u64::from(ttl_secs)).min(MAX_RESOLVE_CACHE_TTL);
                            let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
                            entries.put(key, CacheEntry { target: target.clone(), expires_at: Instant::now() + ttl });
                        }
                    }
                }

                // Notify all coalesced waiters
                let _ = tx.send(Some(result.clone()));
                result
            }
            Role::Follower(mut rx) => {
                // Waiter path: wait for leader
                loop {
                    if let Some(ref res) = *rx.borrow() {
                        return res.clone();
                    }
                    if rx.changed().await.is_err() {
                        return Err("concurrent resolution cancelled".to_string());
                    }
                }
            }
        }
    }
}

struct InFlightGuard<'a> {
    in_flight: &'a InFlightMap,
    key: &'a ResolveKey,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) { self.in_flight.lock().unwrap_or_else(PoisonError::into_inner).remove(self.key); }
}

fn provider<'d>(
    dispatcher: &'d HookDispatcher<'_>,
    plugin: &str,
) -> Result<(&'d Arc<PluginRecord>, &'d LoadedPlugin), String> {
    dispatcher
        .named(PluginHook::MediaFetch, plugin)
        .ok_or_else(|| format!("plugin {plugin} is not enabled or does not provide a media provider"))
}

fn close<T>(
    plugin: &PluginRecord,
    loaded: &LoadedPlugin,
    ctx: &HookCtx<'_>,
    kept: usize,
    started: Instant,
    outcome: &Result<T, CallError>,
) {
    let mut stats = PluginHookStats::new(&plugin.id, PluginHook::MediaFetch);
    stats.kept = count(kept);
    let mut report = PluginRunReport::default();
    let _ = HookDispatcher::settle(
        plugin,
        loaded,
        PluginHook::MediaFetch,
        ctx,
        stats,
        started,
        outcome.as_ref().map(|_| ()),
        &mut report,
    );
}

/// Validate stream target URL and headers according to host security limits.
pub(crate) fn validate_and_screen_stream_target(
    plugin: &str,
    has_network: bool,
    target: WitStreamTarget,
) -> Result<PluginStreamTarget, String> {
    if !has_network {
        return Err(format!("plugin '{plugin}' lacks network capability"));
    }

    let parsed_url = reqwest::Url::parse(&target.url).map_err(|err| format!("invalid target URL: {err}"))?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(format!("invalid stream target scheme '{}': only http and https are allowed", parsed_url.scheme()));
    }
    if parsed_url.host_str().is_none_or(str::is_empty) {
        return Err("stream target URL is missing a valid host".to_string());
    }

    if target.headers.len() > MAX_TARGET_HEADERS {
        return Err(format!(
            "stream target headers count {} exceeds cap of {MAX_TARGET_HEADERS}",
            target.headers.len()
        ));
    }

    let mut total_bytes = 0usize;
    let mut screened_headers = Vec::with_capacity(target.headers.len());
    for (name, value) in target.headers {
        if name.len() > MAX_TARGET_HEADER_NAME_BYTES {
            return Err(format!("stream target header name exceeds limit of {MAX_TARGET_HEADER_NAME_BYTES}"));
        }
        if header_inject::is_denied(&name) {
            return Err(format!("stream target header '{name}' is on the denylist"));
        }
        if value.len() > MAX_TARGET_HEADER_VALUE_BYTES {
            return Err(format!(
                "stream target header value for '{name}' exceeds limit of {MAX_TARGET_HEADER_VALUE_BYTES}"
            ));
        }
        if HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err(format!("stream target header '{name}' is not a valid HTTP header name"));
        }
        if HeaderValue::from_str(&value).is_err() {
            return Err(format!("stream target header value for '{name}' is not a valid HTTP header value"));
        }
        total_bytes = total_bytes.saturating_add(name.len()).saturating_add(value.len());
        if total_bytes > MAX_TARGET_HEADER_TOTAL_BYTES {
            return Err(format!("stream target total header bytes exceed limit of {MAX_TARGET_HEADER_TOTAL_BYTES}"));
        }
        screened_headers.push((name, value));
    }

    Ok(PluginStreamTarget { url: target.url, headers: screened_headers, ttl_secs: target.ttl_secs })
}

async fn call_guest_resolve_stream(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    plugin_id: &str,
    request: &PluginStreamRequest<'_>,
    secrets: &[String],
) -> Result<PluginStreamTarget, CallError> {
    let indices = loaded
        .media_provider
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export media-provider-v1-1".into()))?;
    let exec = loaded.prepared.limits().resolve_exec();
    let mut live = dispatcher.open_with(loaded, exec).await?;
    live.store_mut().data_mut().host.set_call_secrets(secrets.to_vec());
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let limits = live.limits();

    let wit_req = WitStreamRequest {
        input_name: request.input_name.to_string(),
        url: request.url.to_string(),
        username: request.username.map(ToString::to_string),
        password: request.password.map(ToString::to_string),
        options: request.options.to_vec(),
        item_id: request.item_id.to_string(),
        stream_key: request.stream_key.map(ToString::to_string),
        range: request.range.map(ToString::to_string),
    };

    let target = call_with_deadline(limits, guest.call_resolve_stream(live.armed(), &wit_req)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    let target = target?.map_err(|err| CallError::Trap(format!("resolve-stream returned err: {err}")))?;

    validate_and_screen_stream_target(plugin_id, loaded.env.capabilities.network, target).map_err(CallError::Trap)
}

async fn call_guest_resolve_image(
    dispatcher: &HookDispatcher<'_>,
    loaded: &LoadedPlugin,
    plugin_id: &str,
    request: &PluginImageRequest<'_>,
    secrets: &[String],
) -> Result<PluginStreamTarget, CallError> {
    let indices = loaded
        .media_provider
        .as_ref()
        .ok_or_else(|| CallError::Instantiate("does not export media-provider-v1-1".into()))?;
    let exec = loaded.prepared.limits().resolve_exec();
    let mut live = dispatcher.open_with(loaded, exec).await?;
    live.store_mut().data_mut().host.set_call_secrets(secrets.to_vec());
    let instance = live.instance();
    let guest = indices.load(live.store_mut(), &instance).map_err(|err| CallError::Trap(format!("{err:#}")))?;
    let limits = live.limits();

    let wit_req = WitImageRequest {
        input_name: request.input_name.to_string(),
        url: request.url.to_string(),
        username: request.username.map(ToString::to_string),
        password: request.password.map(ToString::to_string),
        options: request.options.to_vec(),
        item_id: request.item_id.to_string(),
        image_key: request.image_key.to_string(),
    };

    let target = call_with_deadline(limits, guest.call_resolve_image(live.armed(), &wit_req)).await;
    host_fns::end_call(live.store_mut().data_mut()).await;
    let target = target?.map_err(|err| CallError::Trap(format!("resolve-image returned err: {err}")))?;

    validate_and_screen_stream_target(plugin_id, loaded.env.capabilities.network, target).map_err(CallError::Trap)
}

/// Resolve stream target with deadline, cache, header screening, and network checks.
pub(crate) async fn resolve_stream(
    dispatcher: &HookDispatcher<'_>,
    cache: &ResolveCache,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginStreamRequest<'_>,
) -> Result<PluginStreamTarget, String> {
    let (record, loaded) = provider(dispatcher, plugin)?;
    let secrets = crate::rpc::collect_call_secrets(
        request.username,
        request.password,
        request.options,
        record.input_form_secret_paths(),
    );
    let key = ResolveKey {
        input: request.input_name.to_string(),
        plugin: Arc::from(plugin),
        item_id: request.item_id.to_string(),
        target: ResolveTargetKey::Stream {
            stream_key: request.stream_key.map(ToString::to_string),
            range: request.range.map(ToString::to_string),
        },
    };
    cache
        .get_or_resolve(key, || async {
            let started = Instant::now();
            let outcome = call_guest_resolve_stream(dispatcher, loaded, &record.id, request, &secrets).await;
            close(record, loaded, ctx, usize::from(outcome.is_ok()), started, &outcome);
            outcome.map_err(|err| err.to_string())
        })
        .await
}

/// Resolve image target with deadline, cache, header screening, and network checks.
pub(crate) async fn resolve_image(
    dispatcher: &HookDispatcher<'_>,
    cache: &ResolveCache,
    plugin: &str,
    ctx: &HookCtx<'_>,
    request: &PluginImageRequest<'_>,
) -> Result<PluginStreamTarget, String> {
    let (record, loaded) = provider(dispatcher, plugin)?;
    let secrets = crate::rpc::collect_call_secrets(
        request.username,
        request.password,
        request.options,
        record.input_form_secret_paths(),
    );
    let key = ResolveKey {
        input: request.input_name.to_string(),
        plugin: Arc::from(plugin),
        item_id: request.item_id.to_string(),
        target: ResolveTargetKey::Image(request.image_key.to_string()),
    };
    cache
        .get_or_resolve(key, || async {
            let started = Instant::now();
            let outcome = call_guest_resolve_image(dispatcher, loaded, &record.id, request, &secrets).await;
            close(record, loaded, ctx, usize::from(outcome.is_ok()), started, &outcome);
            outcome.map_err(|err| err.to_string())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_missing_network_capability() {
        let target = WitStreamTarget { url: "http://example.com/stream.mp4".into(), headers: vec![], ttl_secs: None };
        let err = validate_and_screen_stream_target("my-plugin", false, target).unwrap_err();
        assert!(err.contains("lacks network capability"), "{err}");
    }

    #[test]
    fn validate_rejects_non_http_scheme() {
        for bad_url in ["ftp://example.com/stream.mp4", "file:///etc/passwd", "data:text/plain;base64,abc"] {
            let target = WitStreamTarget { url: bad_url.into(), headers: vec![], ttl_secs: None };
            let err = validate_and_screen_stream_target("my-plugin", true, target).unwrap_err();
            assert!(err.contains("only http and https are allowed"), "{err}");
        }
    }

    #[test]
    fn validate_rejects_missing_or_invalid_host() {
        let target = WitStreamTarget { url: "http://".into(), headers: vec![], ttl_secs: None };
        let err = validate_and_screen_stream_target("my-plugin", true, target).unwrap_err();
        assert!(err.contains("empty host") || err.contains("missing a valid host"), "{err}");
    }

    #[test]
    fn validate_rejects_denylisted_headers() {
        for denied in ["host", "Host", "Connection", "transfer-encoding", "Proxy-Custom"] {
            let target = WitStreamTarget {
                url: "https://example.com/live".into(),
                headers: vec![(denied.into(), "val".into())],
                ttl_secs: None,
            };
            let err = validate_and_screen_stream_target("my-plugin", true, target).unwrap_err();
            assert!(err.contains("on the denylist"), "{err}");
        }
    }

    #[test]
    fn validate_rejects_oversized_header_count() {
        let headers: Vec<(String, String)> = (0..33).map(|i| (format!("X-Header-{i}"), "val".into())).collect();
        let target = WitStreamTarget { url: "https://example.com/live".into(), headers, ttl_secs: None };
        let err = validate_and_screen_stream_target("my-plugin", true, target).unwrap_err();
        assert!(err.contains("exceeds cap of 32"), "{err}");
    }

    #[test]
    fn validate_rejects_oversized_header_bytes() {
        let big_value = "v".repeat(MAX_TARGET_HEADER_VALUE_BYTES + 1);
        let target = WitStreamTarget {
            url: "https://example.com/live".into(),
            headers: vec![("X-Custom".into(), big_value)],
            ttl_secs: None,
        };
        let err = validate_and_screen_stream_target("my-plugin", true, target).unwrap_err();
        assert!(err.contains("exceeds limit"), "{err}");

        let total_big = "v".repeat(4000);
        let headers = (0..5).map(|i| (format!("X-Long-{i}"), total_big.clone())).collect();
        let target2 = WitStreamTarget { url: "https://example.com/live".into(), headers, ttl_secs: None };
        let err2 = validate_and_screen_stream_target("my-plugin", true, target2).unwrap_err();
        assert!(err2.contains("total header bytes exceed limit"), "{err2}");
    }

    #[test]
    fn validate_accepts_valid_target() {
        let target = WitStreamTarget {
            url: "https://example.com/video.mp4".into(),
            headers: vec![("Authorization".into(), "Bearer 123".into()), ("User-Agent".into(), "Custom".into())],
            ttl_secs: Some(300),
        };
        let valid = validate_and_screen_stream_target("my-plugin", true, target).unwrap();
        assert_eq!(valid.url, "https://example.com/video.mp4");
        assert_eq!(valid.headers.len(), 2);
        assert_eq!(valid.ttl_secs, Some(300));
    }

    #[tokio::test]
    async fn cache_coalescing_and_ttl_expiration() {
        let cache = ResolveCache::new();
        let key = ResolveKey {
            input: "input1".into(),
            plugin: Arc::from("plug1"),
            item_id: "item1".into(),
            target: ResolveTargetKey::Stream { stream_key: Some("stream1".into()), range: None },
        };

        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = Arc::clone(&counter);
        let target = cache
            .get_or_resolve(key.clone(), || async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PluginStreamTarget { url: "http://example.com/1".into(), headers: vec![], ttl_secs: Some(1) })
            })
            .await
            .unwrap();
        assert_eq!(target.url, "http://example.com/1");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Immediate second call is cached (counter stays 1)
        let c = Arc::clone(&counter);
        let target2 = cache
            .get_or_resolve(key.clone(), || async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PluginStreamTarget { url: "http://example.com/2".into(), headers: vec![], ttl_secs: Some(1) })
            })
            .await
            .unwrap();
        assert_eq!(target2.url, "http://example.com/1");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Sleep past 1s TTL
        tokio::time::sleep(Duration::from_millis(1100)).await;

        let c = Arc::clone(&counter);
        let target3 = cache
            .get_or_resolve(key.clone(), || async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PluginStreamTarget { url: "http://example.com/2".into(), headers: vec![], ttl_secs: Some(1) })
            })
            .await
            .unwrap();
        assert_eq!(target3.url, "http://example.com/2");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_dropped_leader_does_not_poison_the_key() {
        let cache = ResolveCache::new();
        let key = ResolveKey {
            input: "input1".into(),
            plugin: Arc::from("plug1"),
            item_id: "item1".into(),
            target: ResolveTargetKey::Stream { stream_key: None, range: None },
        };
        let pending = cache.get_or_resolve(key.clone(), std::future::pending);
        assert!(tokio::time::timeout(Duration::from_millis(20), pending).await.is_err(), "leader is cancelled");

        let target = cache
            .get_or_resolve(key, || async {
                Ok(PluginStreamTarget { url: "http://example.com/after".into(), headers: vec![], ttl_secs: None })
            })
            .await
            .expect("a later request resolves again");
        assert_eq!(target.url, "http://example.com/after");
    }

    #[tokio::test]
    async fn cache_does_not_cache_errors_or_zero_ttl() {
        let cache = ResolveCache::new();
        let key = ResolveKey {
            input: "input1".into(),
            plugin: Arc::from("plug1"),
            item_id: "item1".into(),
            target: ResolveTargetKey::Stream { stream_key: None, range: None },
        };

        // Failure
        let res = cache.get_or_resolve(key.clone(), || async { Err("failure".into()) }).await;
        assert!(res.is_err());

        // Second call should run again
        let res2 = cache
            .get_or_resolve(key.clone(), || async {
                Ok(PluginStreamTarget {
                    url: "http://example.com/ok".into(),
                    headers: vec![],
                    ttl_secs: Some(0), // zero TTL: not cached
                })
            })
            .await
            .unwrap();
        assert_eq!(res2.url, "http://example.com/ok");

        // Third call runs again because ttl_secs was 0
        let res3 = cache
            .get_or_resolve(key.clone(), || async {
                Ok(PluginStreamTarget { url: "http://example.com/new".into(), headers: vec![], ttl_secs: None })
            })
            .await
            .unwrap();
        assert_eq!(res3.url, "http://example.com/new");
    }

    #[tokio::test]
    async fn resolve_stream_range_distinguishes_cache_keys() {
        let cache = ResolveCache::new();
        let key_range1 = ResolveKey {
            input: "input1".into(),
            plugin: Arc::from("plug1"),
            item_id: "item1".into(),
            target: ResolveTargetKey::Stream { stream_key: Some("stream1".into()), range: Some("bytes=0-100".into()) },
        };
        let key_range2 = ResolveKey {
            input: "input1".into(),
            plugin: Arc::from("plug1"),
            item_id: "item1".into(),
            target: ResolveTargetKey::Stream {
                stream_key: Some("stream1".into()),
                range: Some("bytes=100-200".into()),
            },
        };

        let target1 = cache
            .get_or_resolve(key_range1, || async {
                Ok(PluginStreamTarget { url: "http://example.com/range1".into(), headers: vec![], ttl_secs: Some(60) })
            })
            .await
            .unwrap();
        let target2 = cache
            .get_or_resolve(key_range2, || async {
                Ok(PluginStreamTarget { url: "http://example.com/range2".into(), headers: vec![], ttl_secs: Some(60) })
            })
            .await
            .unwrap();

        assert_eq!(target1.url, "http://example.com/range1");
        assert_eq!(target2.url, "http://example.com/range2");
    }

    #[tokio::test]
    async fn resolve_cache_clear_drops_cached_entries() {
        let cache = ResolveCache::new();
        let key = ResolveKey {
            input: "input1".into(),
            plugin: Arc::from("plug1"),
            item_id: "item1".into(),
            target: ResolveTargetKey::Stream { stream_key: None, range: None },
        };

        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = Arc::clone(&counter);
        cache
            .get_or_resolve(key.clone(), || async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PluginStreamTarget { url: "http://example.com/1".into(), headers: vec![], ttl_secs: Some(60) })
            })
            .await
            .unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);

        cache.clear();

        let c = Arc::clone(&counter);
        let target = cache
            .get_or_resolve(key, || async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(PluginStreamTarget { url: "http://example.com/2".into(), headers: vec![], ttl_secs: Some(60) })
            })
            .await
            .unwrap();
        assert_eq!(target.url, "http://example.com/2");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
