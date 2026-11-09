use serde::{Deserialize, Serialize};
use tuliprox_plugin_sdk::{kv, prelude::*};

const CACHE_PREFIX: &str = "yt:str:";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedStream {
    pub url: String,
    pub is_live: bool,
    pub instance: String,
}

/// Retrieves a fresh, non-expired stream from the Wasm KV-store,
/// ensuring the cached instance matches one of the actively configured instances.
#[must_use]
pub fn get_stream(video_id: &str, valid_instances: &[String]) -> Option<CachedStream> {
    let key = format!("{CACHE_PREFIX}{video_id}");
    match kv::get_json_fresh::<CachedStream>(&key) {
        Ok(Some(cached)) => {
            // Verify that the cached instance is still in the active instance list
            let clean_cached = cached.instance.trim_end_matches('/');
            let is_valid = valid_instances.iter().any(|inst| {
                let clean_inst = inst.trim_end_matches('/');
                clean_inst == clean_cached
            });

            if is_valid {
                Some(cached)
            } else {
                debug!("invalidating cache entry for {video_id}: instance {} is no longer configured", cached.instance);
                let _ = kv::delete(&key);
                None
            }
        }
        Ok(None) => None,
        Err(err) => {
            debug!("cache lookup error for {key}: {err}");
            None
        }
    }
}

/// Stores a resolved stream into the Wasm KV-store.
/// Live streams are capped at a 10-minute (600s) TTL to detect stream ending/restarts promptly;
/// VOD uploads use the user-configured TTL (10–360 minutes).
pub fn set_stream(video_id: &str, stream: &CachedStream, configured_ttl_minutes: u32) -> Result<(), String> {
    let key = format!("{CACHE_PREFIX}{video_id}");
    let ttl_secs = if stream.is_live {
        600 // 10 minutes max for active live streams
    } else {
        configured_ttl_minutes.clamp(10, 360).saturating_mul(60)
    };
    kv::set_json_ttl(&key, stream, ttl_secs).map_err(|e| e.to_string())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::mock;

    #[test]
    fn cache_live_stream_short_ttl_and_instance_invalidation() {
        mock::reset();
        mock::set_now(1_000);

        let instances = vec!["https://yewtu.be".to_string(), "https://invidious.test".to_string()];
        let live_stream = CachedStream {
            url: "https://yewtu.be/live.m3u8".to_string(),
            is_live: true,
            instance: "https://yewtu.be".to_string(),
        };

        // Store live stream with 120m configured TTL (capped to 600s = 10m internally)
        set_stream("live123", &live_stream, 120).unwrap();

        // Fresh lookup succeeds
        assert_eq!(get_stream("live123", &instances), Some(live_stream.clone()));

        // Lookup with different instance list (yewtu.be removed) invalidates
        let other_instances = vec!["https://invidious.test".to_string()];
        assert_eq!(get_stream("live123", &other_instances), None);

        // Advance 599s -> still valid
        set_stream("live123", &live_stream, 120).unwrap();
        mock::advance(599);
        assert_eq!(get_stream("live123", &instances), Some(live_stream));

        // Advance 2s more (601s total) -> expired
        mock::advance(2);
        assert_eq!(get_stream("live123", &instances), None);
    }
}
