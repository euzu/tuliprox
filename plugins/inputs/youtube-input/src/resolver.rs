use crate::cache::{self, CachedStream};
use serde::Deserialize;
use std::time::Duration;
use tuliprox_plugin_sdk::{clock, http, prelude::*};

const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStream {
    pub url: String,
    pub is_live: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvidiousVideoResponse {
    live_now: Option<bool>,
    hls_url: Option<String>,
    format_streams: Option<Vec<InvidiousFormatStream>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvidiousFormatStream {
    url: String,
    quality_label: Option<String>,
    container: Option<String>,
}

/// Resolves a playable stream for a video using a list of Invidious instances with automatic failover,
/// sticky instance cursor, and wall-clock deadline budget enforcement.
pub fn resolve_stream(
    video_id: &str,
    instances: &[String],
    active_cursor: &mut usize,
    ttl_minutes: u32,
    deadline_unix: i64,
) -> Result<ResolvedStream, String> {
    if instances.is_empty() {
        return Err("no Invidious instances configured".to_string());
    }

    // 1. Check cache first
    if let Some(cached) = cache::get_stream(video_id, instances) {
        debug!("cache hit for video {video_id} (instance: {})", cached.instance);
        return Ok(ResolvedStream { url: cached.url, is_live: cached.is_live });
    }

    debug!("cache miss for video {video_id}, querying Invidious instances");

    // 2. Query instances starting from active_cursor (sticky failover)
    let num_instances = instances.len();
    let mut last_error = String::new();

    for offset in 0..num_instances {
        let now = clock::now_unix();
        if now >= deadline_unix - 2 {
            return Err(format!("time budget exhausted while resolving {video_id} (last error: {last_error})"));
        }

        let idx = (*active_cursor + offset) % num_instances;
        let instance = &instances[idx];
        let clean_instance = instance.trim_end_matches('/');
        // Request with local=true so Invidious proxies streams and prevents IP-bound 403s on client players
        let endpoint = format!("{clean_instance}/api/v1/videos/{video_id}?local=true");

        let remaining_secs = (deadline_unix - now).max(1) as u64;
        let attempt_timeout = remaining_secs.clamp(2, 4);

        match query_invidious_endpoint(&endpoint, clean_instance, attempt_timeout) {
            Ok(resolved) => {
                // Update sticky cursor to the successful instance
                *active_cursor = idx;

                let to_cache = CachedStream {
                    url: resolved.url.clone(),
                    is_live: resolved.is_live,
                    instance: clean_instance.to_string(),
                };
                if let Err(err) = cache::set_stream(video_id, &to_cache, ttl_minutes) {
                    warn!("failed to cache stream URL for {video_id}: {err}");
                }
                return Ok(resolved);
            }
            Err(err) => {
                warn!("invidious query failed for {clean_instance} ({err})");
                last_error = err;
            }
        }
    }

    Err(format!("all Invidious instances failed for video {video_id}. Last error: {last_error}"))
}

fn query_invidious_endpoint(endpoint: &str, instance_base: &str, timeout_secs: u64) -> Result<ResolvedStream, String> {
    let resp = http::Request::get(endpoint)
        .header("User-Agent", BROWSER_UA)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(timeout_secs.clamp(2, 5)))
        .send()
        .map_err(|e| format!("request error: {e}"))?;

    if !resp.is_success() {
        return Err(format!("HTTP status {}", resp.status()));
    }

    let body: InvidiousVideoResponse = resp.json().map_err(|e| format!("JSON decode error: {e}"))?;

    let is_live = body.live_now.unwrap_or(false);

    // Prefer HLS stream
    if let Some(hls) = body.hls_url.filter(|s| !s.is_empty()) {
        let full_url = if hls.starts_with("http://") || hls.starts_with("https://") {
            hls
        } else {
            format!("{instance_base}{hls}")
        };

        // Ensure ?local=true parameter is retained for proxying
        let proxy_url = if !full_url.contains("local=true") {
            if full_url.contains('?') {
                format!("{full_url}&local=true")
            } else {
                format!("{full_url}?local=true")
            }
        } else {
            full_url
        };

        return Ok(ResolvedStream { url: proxy_url, is_live });
    }

    // Direct MP4 fallback
    if let Some(streams) = body.format_streams {
        let preferred = streams
            .iter()
            .find(|s| s.container.as_deref() == Some("mp4") && s.quality_label.as_deref() == Some("720p"))
            .or_else(|| streams.iter().find(|s| s.container.as_deref() == Some("mp4")))
            .or_else(|| streams.first());

        if let Some(stream) = preferred {
            let stream_url = if stream.url.starts_with("http://") || stream.url.starts_with("https://") {
                stream.url.clone()
            } else {
                format!("{instance_base}{}", stream.url)
            };
            return Ok(ResolvedStream { url: stream_url, is_live });
        }
    }

    Err("no playable HLS or MP4 stream found in Invidious response".to_string())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use tuliprox_plugin_sdk::{http::Method, mock};

    #[test]
    fn resolve_stream_failover_and_sticky_cursor() {
        mock::reset();
        mock::set_now(1_000);

        let instances = vec!["https://dead.test".to_string(), "https://good.test".to_string()];
        let mut cursor = 0;

        // Instance 0 fails with 500, instance 1 succeeds with 200
        mock::http_respond(Method::Get, "https://dead.test/api/v1/videos/vid1?local=true", 500, "internal error");
        mock::http_respond(
            Method::Get,
            "https://good.test/api/v1/videos/vid1?local=true",
            200,
            r#"{"liveNow":true,"hlsUrl":"/manifest/live.m3u8"}"#,
        );

        let res = resolve_stream("vid1", &instances, &mut cursor, 120, 1_030).unwrap();
        assert!(res.is_live);
        assert_eq!(res.url, "https://good.test/manifest/live.m3u8?local=true");
        // Cursor is now sticky on index 1!
        assert_eq!(cursor, 1);

        // On the next video, instance 1 is queried directly first (cursor = 1)
        mock::http_respond(
            Method::Get,
            "https://good.test/api/v1/videos/vid2?local=true",
            200,
            r#"{"liveNow":false,"hlsUrl":"","formatStreams":[{"url":"https://good.test/video.mp4","container":"mp4"}]}"#,
        );

        let res2 = resolve_stream("vid2", &instances, &mut cursor, 120, 1_030).unwrap();
        assert!(!res2.is_live);
        assert_eq!(res2.url, "https://good.test/video.mp4");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn resolve_stream_deadline_exhaustion() {
        mock::reset();
        mock::set_now(1_000);

        let instances = vec!["https://slow.test".to_string()];
        let mut cursor = 0;

        // When current time is within 2 seconds of the deadline, resolution is aborted early
        let err = resolve_stream("vid_timeout", &instances, &mut cursor, 120, 1_001).unwrap_err();
        assert!(
            err.contains("time budget exhausted while resolving vid_timeout"),
            "expected deadline exhaustion error, got: {err}"
        );
    }
}
