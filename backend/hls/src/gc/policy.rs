use super::{
    extract_transient_resource_ids, renderer_candidate_window_proxy_seqs, HlsSegmentCache, HlsSession, MapCacheStatus,
    ProxyMapId, SegmentCacheStatus, TransientResourceId, DEFAULT_FAILED_SEGMENT_RETENTION_MS,
    DEFAULT_TEMP_FILE_RETENTION_MS,
};
use std::collections::HashSet;
use tuliprox_core::model::HlsCacheConfig;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GarbageCollectionPolicy {
    pub cache_duration_ms: u64,
    pub cache_bytes_global: u64,
    pub cache_bytes_per_session: u64,
    pub session_idle_timeout_ms: u64,
    pub temp_file_retention_ms: u64,
    pub failed_segment_retention_ms: u64,
}

impl GarbageCollectionPolicy {
    pub fn from_config(config: &HlsCacheConfig) -> Self {
        Self {
            cache_duration_ms: config.cache_duration.as_millis().get(),
            cache_bytes_global: config.cache_bytes.get(),
            cache_bytes_per_session: config.cache_bytes_per_session.get(),
            session_idle_timeout_ms: config.session_idle_timeout.as_millis().get(),
            temp_file_retention_ms: DEFAULT_TEMP_FILE_RETENTION_MS,
            failed_segment_retention_ms: DEFAULT_FAILED_SEGMENT_RETENTION_MS,
        }
    }
}

impl Default for GarbageCollectionPolicy {
    fn default() -> Self {
        let default_config = HlsCacheConfig::from(&shared::model::HlsCacheConfigDto::default());
        Self::from_config(&default_config)
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct ProtectedSet {
    pub segment_proxy_seqs: HashSet<u64>,
    pub map_ids: HashSet<ProxyMapId>,
    pub key_resource_ids: HashSet<TransientResourceId>,
    pub transient_object_ids: HashSet<TransientResourceId>,
}

impl ProtectedSet {
    #[cfg(test)]
    pub fn from_session(session: &HlsSession) -> Self { Self::from_session_for_capacity(session, None) }

    pub(super) fn from_session_for_capacity(session: &HlsSession, release_through: Option<u64>) -> Self {
        let mut protected = Self::default();

        if let Some(rendered) = &session.last_rendered_manifest {
            protected.segment_proxy_seqs.extend(
                rendered
                    .segment_proxy_seqs
                    .iter()
                    .copied()
                    .filter(|proxy_seq| release_through.is_none_or(|release| *proxy_seq > release)),
            );
        }
        if let Some(rendered) = &session.last_rendered_manifest {
            protected.key_resource_ids.extend(extract_transient_resource_ids(&rendered.body));
        }
        for (_, protection) in session.terminal_tail_protections() {
            protected.segment_proxy_seqs.extend(protection.base_proxy_seqs.iter().copied());
        }
        protected.segment_proxy_seqs.extend(
            renderer_candidate_window_proxy_seqs(session)
                .into_iter()
                .filter(|proxy_seq| release_through.is_none_or(|release| *proxy_seq > release)),
        );

        for (proxy_seq, entry) in &session.segments {
            if entry.access.active_readers() > 0
                || matches!(entry.status, SegmentCacheStatus::Fetching { .. })
                || (entry.origin_fetch_ref.is_some()
                    && (matches!(entry.status, SegmentCacheStatus::Queued { .. })
                        || session.segment_prefetch_queue.contains(*proxy_seq)))
            {
                protected.segment_proxy_seqs.insert(*proxy_seq);
            }
        }

        for proxy_seq in &protected.segment_proxy_seqs {
            if let Some(segment) = session.segments.get(proxy_seq) {
                if let Some(map_ref) = segment.map_ref {
                    protected.map_ids.insert(map_ref);
                }
                if let Some(encryption) = &segment.encryption {
                    protected.key_resource_ids.insert(encryption.resource_id.clone());
                }
            }
        }
        for (map_id, map) in &session.maps {
            if map.access.active_readers() > 0
                || matches!(map.status, MapCacheStatus::Queued { .. } | MapCacheStatus::Fetching { .. })
            {
                protected.map_ids.insert(*map_id);
            }
        }
        for entry in session.transient.object_cache.values() {
            if entry.access.active_readers() > 0 {
                protected.transient_object_ids.insert(entry.key.transient_resource_id().clone());
            }
        }
        protected.transient_object_ids.extend(protected.key_resource_ids.iter().cloned());

        protected
    }
}

pub(super) fn protected_set_for_cache_reclamation(
    session: &HlsSession,
    cache: &HlsSegmentCache,
    excluded_path: Option<&std::path::Path>,
    release_through: Option<u64>,
) -> ProtectedSet {
    let mut protected = ProtectedSet::from_session_for_capacity(session, release_through);
    for (proxy_seq, segment) in &session.segments {
        let path = cache.object_path(&segment.cache_key);
        if cache.has_active_mutation(&path) || excluded_path == Some(path.as_path()) {
            protected.segment_proxy_seqs.insert(*proxy_seq);
        }
    }
    for (map_id, map) in &session.maps {
        let path = cache.object_path(&map.cache_key);
        if cache.has_active_mutation(&path) || excluded_path == Some(path.as_path()) {
            protected.map_ids.insert(*map_id);
        }
    }
    for entry in session.transient.object_cache.values() {
        let path = cache.object_path(&entry.key);
        if cache.has_active_mutation(&path) || excluded_path == Some(path.as_path()) {
            protected.transient_object_ids.insert(entry.key.transient_resource_id().clone());
        }
    }
    protected
}
