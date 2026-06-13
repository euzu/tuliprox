use super::{cache::MapCacheKey, ids::ProxySessionId, session::HlsSession, timeline::CacheAccessState};
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

#[derive(Clone, Eq, PartialEq)]
pub struct OriginMapFetchRef {
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
    Failed { failed_at_ms: u64 },
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
