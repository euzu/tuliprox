use super::{MapCacheKey, ProxyMapId, ProxySessionId, SegmentCacheKey, TransientObjectCacheKey, TransientResourceId};
use std::fmt;

impl SegmentCacheKey {
    pub fn new(proxy_session_id: ProxySessionId, proxy_seq: u64, proxy_file_ext: impl Into<String>) -> Self {
        Self { session_id: proxy_session_id, seq: proxy_seq, file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls:{}:{:020}", self.session_id.0, self.seq) }

    pub fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    pub fn proxy_seq(&self) -> u64 { self.seq }
}

impl fmt::Debug for SegmentCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentCacheKey")
            .field("session_id", &"<redacted>")
            .field("seq", &self.seq)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

impl MapCacheKey {
    pub fn new(
        proxy_session_id: ProxySessionId,
        proxy_map_id: impl Into<ProxyMapId>,
        proxy_file_ext: impl Into<String>,
    ) -> Self {
        Self { session_id: proxy_session_id, map_id: proxy_map_id.into(), file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls-map:{}:{:020}", self.session_id.0, self.map_id.0) }

    pub fn proxy_map_id(&self) -> ProxyMapId { self.map_id }
}

impl fmt::Debug for MapCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapCacheKey")
            .field("session_id", &"<redacted>")
            .field("map_id", &self.map_id.0)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

impl TransientObjectCacheKey {
    pub fn new(
        proxy_session_id: ProxySessionId,
        transient_resource_id: TransientResourceId,
        proxy_file_ext: impl Into<String>,
    ) -> Self {
        Self { session_id: proxy_session_id, resource_id: transient_resource_id, file_ext: proxy_file_ext.into() }
    }

    pub fn stable_value(&self) -> String { format!("hls-transient:{}:{}", self.session_id.0, self.resource_id.0) }

    pub fn transient_resource_id(&self) -> &TransientResourceId { &self.resource_id }
}

impl fmt::Debug for TransientObjectCacheKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientObjectCacheKey")
            .field("session_id", &"<redacted>")
            .field("resource_id", &self.resource_id)
            .field("file_ext", &self.file_ext)
            .finish()
    }
}

/// Resolves a proxy cache object to a safe path below the configured HLS cache root.
pub trait HlsCacheObjectKey {
    fn proxy_session_id(&self) -> &ProxySessionId;
    fn session_path_component(&self) -> String;
    fn file_name(&self) -> String;
}

impl HlsCacheObjectKey for SegmentCacheKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("{:06}.{}", self.seq, self.file_ext) }
}

impl HlsCacheObjectKey for MapCacheKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("map/{:06}.{}", self.map_id.0, self.file_ext) }
}

impl HlsCacheObjectKey for TransientObjectCacheKey {
    fn proxy_session_id(&self) -> &ProxySessionId { &self.session_id }

    fn session_path_component(&self) -> String {
        let value = &self.session_id.0;
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
            return value.clone();
        }
        blake3::hash(value.as_bytes()).to_hex().to_string()
    }

    fn file_name(&self) -> String { format!("r/{}.{}", self.resource_id.0, self.file_ext) }
}
