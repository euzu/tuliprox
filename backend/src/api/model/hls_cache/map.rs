use super::{cache::MapCacheKey, ids::ProxySessionId, session::HlsSession, timeline::CacheAccessState};
use axum::http::StatusCode;
use crate::processing::parser::hls::origin_manifest::ParsedByteRange;
use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

/// Proxy-visible identifier for one EXT-X-MAP cache resource.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ProxyMapId(pub u64);

impl From<u64> for ProxyMapId {
    fn from(value: u64) -> Self { Self(value) }
}

#[derive(Clone, Eq, PartialEq)]
pub struct OriginMapKey {
    pub origin_epoch: u64,
    /// Concrete absolute MAP fetch URI resolved against the final manifest URL.
    ///
    /// This may intentionally contain a provider mirror, redirect target, or CDN
    /// host. Do not normalize it back to `provider://` or the original manifest
    /// host unless a separate semantic identity is introduced and refetch safety
    /// for relative EXT-X-MAP URIs is proven.
    pub resolved_origin_uri: String,
    pub byte_range: Option<ParsedByteRange>,
}

impl fmt::Debug for OriginMapKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginMapKey")
            .field("origin_epoch", &self.origin_epoch)
            .field("resolved_origin_uri", &"<redacted>")
            .field("byte_range", &self.byte_range)
            .finish()
    }
}

impl Hash for OriginMapKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.origin_epoch.hash(state);
        self.resolved_origin_uri.hash(state);
        match self.byte_range {
            Some(byte_range) => {
                1_u8.hash(state);
                byte_range.length.hash(state);
                byte_range.offset.hash(state);
            }
            None => 0_u8.hash(state),
        }
    }
}

/// Volatile concrete origin URL for one EXT-X-MAP download.
///
/// The URL is resolved against the final fetched manifest URL and may include a redirect/CDN host. Use it only as a
/// fetch target or sanitized diagnostics; stable proxy cache identity is `MapCacheKey`.
#[derive(Clone, Eq, PartialEq)]
pub struct OriginMapFetchRef {
    /// Concrete URL used to refetch the MAP object.
    ///
    /// Keep this aligned with `OriginMapKey::resolved_origin_uri`; relative MAP
    /// URIs must remain resolved against the final manifest URL after redirects.
    pub resolved_origin_url: String,
    pub byte_range: Option<ParsedByteRange>,
    pub valid_until_ms: Option<u64>,
}

impl OriginMapFetchRef {
    pub fn is_valid_at(&self, now_ms: u64) -> bool {
        self.valid_until_ms.is_none_or(|valid_until_ms| now_ms <= valid_until_ms)
    }
}

impl fmt::Debug for OriginMapFetchRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginMapFetchRef")
            .field("resolved_origin_url", &"<redacted>")
            .field("byte_range", &self.byte_range)
            .field("valid_until_ms", &self.valid_until_ms)
            .finish()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MapCacheStatus {
    Discovered,
    Queued { queued_at_ms: u64 },
    Fetching { started_at_ms: u64 },
    Ready { content_length: u64, ready_at_ms: u64 },
    FailedRetryable { failed_at_ms: u64, retry_after_ms: u64 },
    FailedPermanent { failed_at_ms: u64, status: Option<StatusCode> },
    Expired,
}

#[derive(Clone, Eq, PartialEq)]
pub struct MapEntry {
    pub proxy_map_id: ProxyMapId,
    pub origin_key: OriginMapKey,
    pub proxy_file_ext: String,
    pub content_type: String,
    pub cache_key: MapCacheKey,
    pub origin_fetch_ref: Option<OriginMapFetchRef>,
    pub byte_range: Option<ParsedByteRange>,
    pub status: MapCacheStatus,
    pub access: Arc<CacheAccessState>,
}

impl fmt::Debug for MapEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapEntry")
            .field("proxy_map_id", &self.proxy_map_id)
            .field("origin_key", &self.origin_key)
            .field("proxy_file_ext", &self.proxy_file_ext)
            .field("content_type", &self.content_type)
            .field("cache_key", &self.cache_key)
            .field("origin_fetch_ref", &self.origin_fetch_ref)
            .field("byte_range", &self.byte_range)
            .field("status", &self.status)
            .field("active_readers", &self.access.active_readers())
            .field("last_accessed_at_ms", &self.access.last_accessed_at_ms())
            .finish()
    }
}

impl MapEntry {
    pub fn default_content_type() -> &'static str { "video/mp4" }

    /// Creates a proxy MAP entry from a concrete origin MAP key.
    ///
    /// `origin_fetch_ref` intentionally starts from `origin_key.resolved_origin_uri`.
    /// Until a separate semantic MAP identity exists, this preserves the final
    /// redirect/CDN fetch URL required to reload relative EXT-X-MAP resources.
    pub fn new(
        proxy_session_id: &ProxySessionId,
        proxy_map_id: ProxyMapId,
        origin_key: OriginMapKey,
        proxy_file_ext: String,
    ) -> Self {
        Self {
            proxy_map_id,
            origin_fetch_ref: Some(OriginMapFetchRef {
                resolved_origin_url: origin_key.resolved_origin_uri.clone(),
                byte_range: origin_key.byte_range,
                valid_until_ms: None,
            }),
            byte_range: origin_key.byte_range,
            cache_key: MapCacheKey::new(proxy_session_id.clone(), proxy_map_id, &proxy_file_ext),
            content_type: Self::default_content_type().to_string(),
            proxy_file_ext,
            origin_key,
            status: MapCacheStatus::Discovered,
            access: Arc::new(CacheAccessState::new()),
        }
    }
}

impl HlsSession {
    pub fn queue_map_fetch_candidates(&mut self, now_ms: u64) {
        if self.is_gc_marked_for_removal() {
            return;
        }
        for map in self.maps.values_mut() {
            if matches!(map.status, MapCacheStatus::Discovered) && map.origin_fetch_ref.is_some() {
                map.status = MapCacheStatus::Queued { queued_at_ms: now_ms };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MapEntry, OriginMapKey, ParsedByteRange, ProxyMapId};
    use crate::api::model::ProxySessionId;

    #[test]
    fn map_entry_preserves_concrete_fetch_url_and_byte_range() {
        let key = OriginMapKey {
            origin_epoch: 0,
            resolved_origin_uri: "https://cdn.example.net/live/redirected/init.mp4".to_string(),
            byte_range: Some(ParsedByteRange { length: 100, offset: 50 }),
        };

        let entry = MapEntry::new(&ProxySessionId("proxy".to_string()), ProxyMapId(7), key.clone(), "mp4".to_string());

        assert_eq!(entry.origin_key, key);
        let fetch_ref = entry.origin_fetch_ref.as_ref().expect("fetch ref");
        assert_eq!(fetch_ref.resolved_origin_url, "https://cdn.example.net/live/redirected/init.mp4");
        assert_eq!(fetch_ref.byte_range, Some(ParsedByteRange { length: 100, offset: 50 }));
    }
}
