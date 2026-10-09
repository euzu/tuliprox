use super::{
    deletion::{CacheObjectDeletion, SegmentCacheDeletionReason},
    policy::protected_set_for_cache_reclamation,
    CacheDeletionBatch, CapacityReclamationEvidence, GarbageCollectionPolicy, GlobalSegmentCandidate,
    GlobalTransientObjectCandidate, HlsSegmentCache, HlsSession, HlsSessionHandle, MapCacheStatus, MapEntryDeletion,
    ProtectedSet, ProxyMapId, ProxySessionId, SegmentCacheKey, SegmentCacheStatus, SegmentDeleteCandidate,
    TransientObjectCacheKey, TransientResourceId,
};
use std::{collections::HashSet, sync::Arc};

pub(super) async fn total_sessions_cache_size(sessions: &[HlsSessionHandle]) -> u64 {
    let mut total_size = 0_u64;
    for session in sessions {
        let session = session.read().await;
        total_size = total_size.saturating_add(session_cache_size(&session));
    }
    total_size
}

pub(super) fn collect_projected_session_reclamation(
    session: &mut HlsSession,
    cache: &HlsSegmentCache,
    excluded_path: &std::path::Path,
    release_through: Option<u64>,
    required_bytes: u64,
    max_deletions: usize,
    deletions: &mut CacheDeletionBatch,
) {
    let mut planned_bytes = 0_u64;
    let mut planned_deletions = 0_usize;
    let mut transient_readiness_changed = false;
    while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
        let protected = protected_set_for_cache_reclamation(session, cache, Some(excluded_path), release_through);
        let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.transient_object_ids) else {
            break;
        };
        planned_bytes = planned_bytes.saturating_add(removal.content_length);
        transient_readiness_changed = true;
        deletions
            .push(CacheObjectDeletion::TransientObject { key: removal.key, content_length: removal.content_length });
        planned_deletions = planned_deletions.saturating_add(1);
    }
    while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
        let protected = protected_set_for_cache_reclamation(session, cache, Some(excluded_path), release_through);
        let Some(candidate) = fifo_head_size_candidate(session, &protected) else {
            break;
        };
        if let Some(deletion) = remove_segment_entry(session, candidate.proxy_seq) {
            planned_bytes = planned_bytes.saturating_add(candidate.content_length);
            deletions
                .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::SessionSize });
            planned_deletions = planned_deletions.saturating_add(1);
        }
    }
    for map_id in unprotected_unreferenced_map_ids(
        session,
        &protected_set_for_cache_reclamation(session, cache, Some(excluded_path), release_through),
    ) {
        if planned_bytes >= required_bytes || planned_deletions >= max_deletions || !deletions.has_capacity() {
            break;
        }
        if let Some(deletion) = remove_map_entry(session, map_id) {
            planned_bytes = planned_bytes.saturating_add(deletion.content_length);
            deletions.push(CacheObjectDeletion::Map(deletion.key));
            planned_deletions = planned_deletions.saturating_add(1);
        }
    }
    if transient_readiness_changed {
        session.advance_media_readiness_generation();
    }
}

pub(super) async fn oldest_global_fifo_head_candidate(
    sessions: &[HlsSessionHandle],
    cache: &HlsSegmentCache,
    excluded_path: Option<&std::path::Path>,
    skipped_sessions: &HashSet<ProxySessionId>,
) -> Option<GlobalSegmentCandidate> {
    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if skipped_sessions.contains(&session_guard.proxy_session_id) {
            continue;
        }
        if let Some(candidate) = fifo_head_size_candidate(
            &session_guard,
            &protected_set_for_cache_reclamation(&session_guard, cache, excluded_path, None),
        ) {
            candidates.push(GlobalSegmentCandidate {
                session: Arc::clone(session),
                proxy_session_id: session_guard.proxy_session_id.clone(),
                proxy_seq: candidate.proxy_seq,
                content_length: candidate.content_length,
                last_relevant_at_ms: candidate.last_relevant_at_ms,
            });
        }
    }
    candidates.into_iter().min_by_key(|candidate| (candidate.last_relevant_at_ms, candidate.proxy_seq))
}

pub(super) async fn oldest_global_transient_object_candidate(
    sessions: &[HlsSessionHandle],
    cache: &HlsSegmentCache,
    excluded_path: Option<&std::path::Path>,
    skipped_sessions: &HashSet<ProxySessionId>,
) -> Option<GlobalTransientObjectCandidate> {
    let mut candidates = Vec::new();
    for session in sessions {
        let session_guard = session.read().await;
        if skipped_sessions.contains(&session_guard.proxy_session_id) {
            continue;
        }
        let protected = protected_set_for_cache_reclamation(&session_guard, cache, excluded_path, None);
        candidates.extend(session_guard.transient.object_cache.values().filter_map(|entry| {
            entry.ready_content_length()?;
            if entry.access.active_readers() > 0
                || protected.transient_object_ids.contains(entry.key.transient_resource_id())
            {
                return None;
            }
            Some(GlobalTransientObjectCandidate {
                session: Arc::clone(session),
                proxy_session_id: session_guard.proxy_session_id.clone(),
                last_accessed_at_ms: entry.last_accessed_at_ms,
            })
        }));
    }
    candidates.into_iter().min_by_key(|candidate| candidate.last_accessed_at_ms)
}

pub(super) fn take_expired_transient_objects(
    session: &mut HlsSession,
    now_ms: u64,
    protected: &HashSet<TransientResourceId>,
    limit: usize,
) -> Vec<(TransientObjectCacheKey, u64)> {
    session
        .transient
        .take_expired_object_removals_except(now_ms, protected, limit)
        .into_iter()
        .map(|removal| (removal.key, removal.content_length))
        .collect()
}

pub(super) fn remove_stale_queue_entries(session: &mut HlsSession) -> usize {
    let mut removed = 0_usize;
    for proxy_seq in session.segment_prefetch_queue.proxy_seqs() {
        let stale = session.segments.get(&proxy_seq).is_none_or(|segment| {
            segment.origin_fetch_ref.is_none() || !matches!(segment.status, SegmentCacheStatus::Queued { .. })
        });
        if stale && session.segment_prefetch_queue.remove(proxy_seq).is_some() {
            removed = removed.saturating_add(1);
        }
    }
    removed
}

pub(super) fn duration_expired_head_segment(
    session: &HlsSession,
    protected: &ProtectedSet,
    policy: &GarbageCollectionPolicy,
    now_ms: u64,
) -> Option<u64> {
    let (proxy_seq, segment) = session.segments.iter().next()?;
    if protected.segment_proxy_seqs.contains(proxy_seq) {
        return None;
    }
    let last_relevant_at_ms = segment_last_relevant_at_ms(segment)?;
    let retention_ms = match segment.status {
        SegmentCacheStatus::FailedRetryable { .. }
        | SegmentCacheStatus::FailedPermanent { .. }
        | SegmentCacheStatus::Expired => policy.failed_segment_retention_ms,
        SegmentCacheStatus::Ready { .. } => policy
            .cache_duration_ms
            .max(segment.duration_ms.saturating_add(session.longest_rendered_playlist_duration_ms)),
        SegmentCacheStatus::Discovered
        | SegmentCacheStatus::Queued { .. }
        | SegmentCacheStatus::Fetching { .. }
        | SegmentCacheStatus::CapacityDeferred { .. } => return None,
    };
    (now_ms.saturating_sub(last_relevant_at_ms) >= retention_ms).then_some(*proxy_seq)
}

pub(super) fn fifo_head_size_candidate(
    session: &HlsSession,
    protected: &ProtectedSet,
) -> Option<SegmentDeleteCandidate> {
    let (proxy_seq, segment) = session.segments.iter().next()?;
    if protected.segment_proxy_seqs.contains(proxy_seq) {
        return None;
    }
    let SegmentCacheStatus::Ready { content_length, .. } = segment.status else {
        return None;
    };
    Some(SegmentDeleteCandidate {
        proxy_seq: *proxy_seq,
        content_length,
        last_relevant_at_ms: segment_last_relevant_at_ms(segment).unwrap_or_default(),
    })
}

fn segment_last_relevant_at_ms(segment: &super::super::SegmentEntry) -> Option<u64> {
    let status_at = match segment.status {
        SegmentCacheStatus::Ready { ready_at_ms, .. } => Some(ready_at_ms),
        SegmentCacheStatus::FailedRetryable { failed_at_ms, .. }
        | SegmentCacheStatus::FailedPermanent { failed_at_ms, .. } => Some(failed_at_ms),
        SegmentCacheStatus::Expired => segment.last_rendered_at_ms,
        SegmentCacheStatus::Discovered
        | SegmentCacheStatus::Queued { .. }
        | SegmentCacheStatus::Fetching { .. }
        | SegmentCacheStatus::CapacityDeferred { .. } => None,
    };
    [status_at, segment.last_rendered_at_ms, Some(segment.access.last_accessed_at_ms()).filter(|value| *value > 0)]
        .into_iter()
        .flatten()
        .max()
}

pub(super) fn session_cache_size(session: &HlsSession) -> u64 {
    let segment_bytes = session
        .segments
        .values()
        .map(|segment| match segment.status {
            SegmentCacheStatus::Ready { content_length, .. } => content_length,
            _ => 0,
        })
        .sum::<u64>();
    let map_bytes = session
        .maps
        .values()
        .map(|map| match map.status {
            MapCacheStatus::Ready { content_length, .. } => content_length,
            _ => 0,
        })
        .sum::<u64>();
    segment_bytes.saturating_add(map_bytes).saturating_add(session.transient.ready_object_cache_size())
}

pub(super) fn capacity_reclamation_evidence(
    session: &HlsSession,
    protected: &ProtectedSet,
) -> CapacityReclamationEvidence {
    let protected_segment_bytes = session.segments.iter().fold(0_u64, |bytes, (proxy_seq, segment)| {
        if !protected.segment_proxy_seqs.contains(proxy_seq) {
            return bytes;
        }
        match segment.status {
            SegmentCacheStatus::Ready { content_length, .. } => bytes.saturating_add(content_length),
            _ => bytes,
        }
    });
    let protected_map_bytes = session.maps.iter().fold(0_u64, |bytes, (map_id, map)| {
        if !protected.map_ids.contains(map_id) {
            return bytes;
        }
        match map.status {
            MapCacheStatus::Ready { content_length, .. } => bytes.saturating_add(content_length),
            _ => bytes,
        }
    });
    let (protected_transient_object_bytes, reclaimable_transient_object_bytes) =
        session.transient.object_cache.values().fold((0_u64, 0_u64), |(protected_bytes, reclaimable_bytes), entry| {
            let Some(content_length) = entry.ready_content_length() else {
                return (protected_bytes, reclaimable_bytes);
            };
            if protected.transient_object_ids.contains(entry.key.transient_resource_id())
                || entry.access.active_readers() > 0
            {
                (protected_bytes.saturating_add(content_length), reclaimable_bytes)
            } else {
                (protected_bytes, reclaimable_bytes.saturating_add(content_length))
            }
        });
    let reclaimable_bytes = fifo_head_size_candidate(session, protected)
        .map_or(0, |candidate| candidate.content_length)
        .saturating_add(reclaimable_transient_object_bytes);
    CapacityReclamationEvidence {
        protected_working_set_bytes: protected_segment_bytes
            .saturating_add(protected_map_bytes)
            .saturating_add(protected_transient_object_bytes),
        reclaimable_bytes,
    }
}

pub(super) fn remove_segment_entry(session: &mut HlsSession, proxy_seq: u64) -> Option<SegmentCacheKey> {
    let segment = session.segments.remove(&proxy_seq)?;
    if let Some(startup) = &mut session.startup {
        startup.revisions.remove(&proxy_seq);
    }
    let removed_ready_media = matches!(segment.status, SegmentCacheStatus::Ready { .. });
    session.segment_prefetch_queue.remove(proxy_seq);
    session.origin_to_proxy.retain(|_, mapped_seq| *mapped_seq != proxy_seq);
    if segment.discontinuity_before {
        session.discontinuity_sequence = session.discontinuity_sequence.saturating_add(1);
    }
    if removed_ready_media {
        session.advance_media_readiness_generation();
    }
    if session.publishable_origin_head_proxy_seq == Some(proxy_seq) {
        session.publishable_origin_head_proxy_seq = session
            .segments
            .range(proxy_seq.saturating_add(1)..)
            .find_map(|(next_proxy_seq, segment)| segment.origin_fetch_ref.as_ref().map(|_| *next_proxy_seq));
    }
    Some(segment.cache_key)
}

pub(super) fn unprotected_unreferenced_map_ids(session: &HlsSession, protected: &ProtectedSet) -> Vec<ProxyMapId> {
    let referenced = session.segments.values().filter_map(|segment| segment.map_ref).collect::<HashSet<_>>();
    session
        .maps
        .iter()
        .filter_map(|(map_id, map)| {
            if referenced.contains(map_id)
                || protected.map_ids.contains(map_id)
                || map.access.active_readers() > 0
                || matches!(map.status, MapCacheStatus::Queued { .. } | MapCacheStatus::Fetching { .. })
            {
                return None;
            }
            Some(*map_id)
        })
        .collect()
}

pub(super) fn remove_map_entry(session: &mut HlsSession, map_id: ProxyMapId) -> Option<MapEntryDeletion> {
    let map = session.maps.remove(&map_id)?;
    let (content_length, removed_ready_media) = match map.status {
        MapCacheStatus::Ready { content_length, .. } => (content_length, true),
        _ => (0, false),
    };
    if removed_ready_media {
        session.advance_media_readiness_generation();
    }
    session.origin_map_to_proxy.retain(|_, mapped_map_id| *mapped_map_id != map_id);
    Some(MapEntryDeletion { key: map.cache_key, content_length })
}
