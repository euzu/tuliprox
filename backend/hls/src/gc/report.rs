use super::ProxySessionId;

#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct GarbageCollectionReport {
    pub secret_cache_invalidated: bool,
    pub secret_cache_invalidation_deferred: bool,
    pub temp_files_deleted: usize,
    pub orphan_session_dirs_deleted: usize,
    pub stale_queue_entries_removed: usize,
    pub segments_deleted_duration: usize,
    pub segments_deleted_size_session: usize,
    pub segments_deleted_size_global: usize,
    pub maps_deleted: usize,
    pub sessions_deleted: usize,
    pub removed_session_ids: Vec<ProxySessionId>,
    pub transient_resources_pruned: usize,
    pub transient_objects_deleted: usize,
    pub transient_object_bytes_deleted: u64,
    pub cache_object_deletions_planned: usize,
    pub cache_object_deletions_succeeded: usize,
    pub cache_object_deletions_deferred: usize,
}

impl GarbageCollectionReport {
    pub(super) fn segments_deleted(&self) -> usize {
        self.segments_deleted_duration
            .saturating_add(self.segments_deleted_size_session)
            .saturating_add(self.segments_deleted_size_global)
    }

    pub fn did_cleanup_or_invalidate(&self) -> bool {
        self.secret_cache_invalidated
            || self.secret_cache_invalidation_deferred
            || self.temp_files_deleted > 0
            || self.orphan_session_dirs_deleted > 0
            || self.stale_queue_entries_removed > 0
            || self.segments_deleted() > 0
            || self.maps_deleted > 0
            || self.sessions_deleted > 0
            || self.transient_resources_pruned > 0
            || self.transient_objects_deleted > 0
            || self.cache_object_deletions_planned > 0
            || self.cache_object_deletions_succeeded > 0
            || self.cache_object_deletions_deferred > 0
    }
}
