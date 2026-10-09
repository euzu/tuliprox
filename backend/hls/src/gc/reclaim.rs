use super::{
    deletion::{CacheObjectDeletion, SegmentCacheDeletionReason},
    policy::protected_set_for_cache_reclamation,
    selection::{
        capacity_reclamation_evidence, collect_projected_session_reclamation, duration_expired_head_segment,
        fifo_head_size_candidate, oldest_global_fifo_head_candidate, oldest_global_transient_object_candidate,
        remove_map_entry, remove_segment_entry, remove_stale_queue_entries, session_cache_size,
        take_expired_transient_objects, total_sessions_cache_size, unprotected_unreferenced_map_ids,
    },
    CacheDeletionBatch, GarbageCollectionPolicy, GarbageCollectionReport, HlsCacheCapacityReclaimOutcome,
    HlsCacheCapacityReclaimRequest, HlsGarbageCollector, HlsSegmentCache, HlsSession, HlsSessionHandle,
    MAX_CACHE_DELETE_RETRIES_PER_RUN,
};
use std::{
    collections::HashSet,
    io,
    sync::{Arc, Weak},
};

impl HlsGarbageCollector {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn reclaim_for_projected_write(
        &self,
        request: HlsCacheCapacityReclaimRequest,
    ) -> io::Result<HlsCacheCapacityReclaimOutcome> {
        let _run_once = self.run_once_gate.lock().await;
        if !self.cache.contains_current_cache_path(&request.target_path) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "hls cache path changed before capacity reclamation",
            ));
        }
        let before = self.cache.capacity_usage(&request.proxy_session_id).await?;
        let mut report = GarbageCollectionReport::default();
        let mut deletion_attempt_budget = MAX_CACHE_DELETE_RETRIES_PER_RUN;
        let mut protected_working_set_bytes = 0_u64;
        let mut reclaimable_bytes = 0_u64;

        let mut pending_attempt_budget = deletion_attempt_budget / 2;
        let pending_attempts_before = pending_attempt_budget;
        self.retry_pending_cache_deletions(&mut report, &mut pending_attempt_budget).await;
        deletion_attempt_budget =
            deletion_attempt_budget.saturating_sub(pending_attempts_before.saturating_sub(pending_attempt_budget));
        let after_pending = self.cache.capacity_usage(&request.proxy_session_id).await?;
        let required_session_bytes = request
            .required_session_bytes
            .saturating_sub(before.session_bytes.saturating_sub(after_pending.session_bytes));
        if required_session_bytes > 0 {
            if let Some(session) = self.sessions.get_by_proxy_session_id(&request.proxy_session_id).await {
                let mut deletions = self.reserve_cache_deletion_batch();
                let access_leases = self
                    .access_leases
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                    .and_then(Weak::upgrade);
                let lease_guard = match &access_leases {
                    Some(access_leases) => Some(access_leases.read().await),
                    None => None,
                };
                // Projected-capacity selection follows the runtime lock order
                // lease store -> session. The session lock is non-blocking
                // while the lease snapshot is frozen, and neither guard
                // crosses filesystem I/O or an await. Contention installs a
                // bounded wake task instead of reversing the lock order.
                let release_through =
                    lease_guard.as_ref().and_then(|leases| leases.capacity_release_through(&request.proxy_session_id));
                let mut session_lock_busy = false;
                if let Ok(mut session) = session.try_write() {
                    let protected = protected_set_for_cache_reclamation(
                        &session,
                        &self.cache,
                        Some(&request.target_path),
                        release_through,
                    );
                    let evidence = capacity_reclamation_evidence(&session, &protected);
                    protected_working_set_bytes = evidence.protected_working_set_bytes;
                    reclaimable_bytes = evidence.reclaimable_bytes;
                    collect_projected_session_reclamation(
                        &mut session,
                        &self.cache,
                        &request.target_path,
                        release_through,
                        required_session_bytes,
                        deletion_attempt_budget,
                        &mut deletions,
                    );
                    if !deletions.deletions.is_empty() {
                        let rendered_at_ms = session
                            .last_rendered_manifest
                            .as_ref()
                            .map_or(session.last_client_access_at_ms, |rendered| {
                                rendered.rendered_at_ms.saturating_add(1)
                            });
                        let _ = session.render_and_store_manifest(rendered_at_ms);
                    }
                } else {
                    protected_working_set_bytes = after_pending.session_bytes;
                    session_lock_busy = true;
                }
                drop(lease_guard);
                if session_lock_busy {
                    let wake_session = Arc::clone(&session);
                    let wake_cache = Arc::clone(&self.cache);
                    tokio::spawn(async move {
                        let guard = wake_session.write().await;
                        drop(guard);
                        wake_cache.notify_capacity_protection_changed();
                    });
                }
                deletions.execute_prioritized(&self.cache, &mut report, &mut deletion_attempt_budget).await;
            }
        }

        let after_session = self.cache.capacity_usage(&request.proxy_session_id).await?;
        let remaining_session_bytes = request
            .required_session_bytes
            .saturating_sub(before.session_bytes.saturating_sub(after_session.session_bytes));
        if remaining_session_bytes > 0 {
            return Ok(HlsCacheCapacityReclaimOutcome {
                reclaimed_session_bytes: before.session_bytes.saturating_sub(after_session.session_bytes),
                reclaimed_global_bytes: before.global_bytes.saturating_sub(after_session.global_bytes),
                protected_working_set_bytes,
                reclaimable_bytes,
            });
        }
        let required_global_bytes = request
            .required_global_bytes
            .saturating_sub(before.global_bytes.saturating_sub(after_session.global_bytes));
        if required_global_bytes > 0 {
            let sessions = self.sessions.list_sessions().await;
            let mut deletions = self.reserve_cache_deletion_batch();
            self.collect_projected_global_reclamation(
                &sessions,
                &request.target_path,
                required_global_bytes,
                deletion_attempt_budget,
                &mut deletions,
            )
            .await;
            deletions.execute_prioritized(&self.cache, &mut report, &mut deletion_attempt_budget).await;
        }

        let after = self.cache.capacity_usage(&request.proxy_session_id).await?;
        Ok(HlsCacheCapacityReclaimOutcome {
            reclaimed_session_bytes: before.session_bytes.saturating_sub(after.session_bytes),
            reclaimed_global_bytes: before.global_bytes.saturating_sub(after.global_bytes),
            protected_working_set_bytes,
            reclaimable_bytes,
        })
    }

    pub(super) fn collect_session_deletions(
        session: &mut HlsSession,
        cache: &HlsSegmentCache,
        now_ms: u64,
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
        deletions: &mut CacheDeletionBatch,
    ) {
        report.stale_queue_entries_removed =
            report.stale_queue_entries_removed.saturating_add(remove_stale_queue_entries(session));

        let transient_before = session.transient.resources.len();
        let protected = protected_set_for_cache_reclamation(session, cache, None, None);
        session.transient.prune_expired_except(now_ms, &protected.key_resource_ids);
        let transient_resources_pruned = transient_before.saturating_sub(session.transient.resources.len());
        let mut transient_readiness_changed = transient_resources_pruned > 0;
        report.transient_resources_pruned =
            report.transient_resources_pruned.saturating_add(transient_resources_pruned);

        let expired_transient_objects = take_expired_transient_objects(
            session,
            now_ms,
            &protected.transient_object_ids,
            deletions.remaining_capacity(),
        );
        transient_readiness_changed |= !expired_transient_objects.is_empty();
        for (key, content_length) in expired_transient_objects {
            deletions.push(CacheObjectDeletion::TransientObject { key, content_length });
        }

        while deletions.has_capacity() {
            let Some(proxy_seq) = duration_expired_head_segment(
                session,
                &protected_set_for_cache_reclamation(session, cache, None, None),
                policy,
                now_ms,
            ) else {
                break;
            };
            if let Some(deletion) = remove_segment_entry(session, proxy_seq) {
                deletions
                    .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::Duration });
            }
        }

        let mut session_size = session_cache_size(session);
        while session_size > policy.cache_bytes_per_session && deletions.has_capacity() {
            let protected = protected_set_for_cache_reclamation(session, cache, None, None);
            let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.transient_object_ids)
            else {
                break;
            };
            session_size = session_size.saturating_sub(removal.content_length);
            transient_readiness_changed = true;
            deletions.push(CacheObjectDeletion::TransientObject {
                key: removal.key,
                content_length: removal.content_length,
            });
        }
        while session_size > policy.cache_bytes_per_session && deletions.has_capacity() {
            let Some(candidate) =
                fifo_head_size_candidate(session, &protected_set_for_cache_reclamation(session, cache, None, None))
            else {
                break;
            };
            session_size = session_size.saturating_sub(candidate.content_length);
            if let Some(deletion) = remove_segment_entry(session, candidate.proxy_seq) {
                deletions.push(CacheObjectDeletion::Segment {
                    key: deletion,
                    reason: SegmentCacheDeletionReason::SessionSize,
                });
            }
        }

        for map_id in
            unprotected_unreferenced_map_ids(session, &protected_set_for_cache_reclamation(session, cache, None, None))
        {
            if !deletions.has_capacity() {
                break;
            }
            if let Some(deletion) = remove_map_entry(session, map_id) {
                session_size = session_size.saturating_sub(deletion.content_length);
                deletions.push(CacheObjectDeletion::Map(deletion.key));
            }
        }
        if transient_readiness_changed {
            session.advance_media_readiness_generation();
        }
    }

    pub(super) async fn collect_global_size_deletions(
        &self,
        sessions: &[HlsSessionHandle],
        policy: &GarbageCollectionPolicy,
        report: &mut GarbageCollectionReport,
        deletions: &mut CacheDeletionBatch,
    ) {
        let mut total_size = total_sessions_cache_size(sessions).await;
        let mut transient_readiness_advanced = HashSet::new();
        let mut skipped_transient_sessions = HashSet::new();

        loop {
            if total_size <= policy.cache_bytes_global || !deletions.has_capacity() {
                break;
            }
            let Some(candidate) =
                oldest_global_transient_object_candidate(sessions, &self.cache, None, &skipped_transient_sessions)
                    .await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let protected = protected_set_for_cache_reclamation(&session, &self.cache, None, None);
            let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.transient_object_ids)
            else {
                skipped_transient_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            total_size = total_size.saturating_sub(removal.content_length);
            if transient_readiness_advanced.insert(session.proxy_session_id.clone()) {
                session.advance_media_readiness_generation();
            }
            deletions.push(CacheObjectDeletion::TransientObject {
                key: removal.key,
                content_length: removal.content_length,
            });
            deletions.persist_pending(report);
        }

        let mut skipped_segment_sessions = HashSet::new();
        loop {
            if total_size <= policy.cache_bytes_global || !deletions.has_capacity() {
                break;
            }
            let Some(candidate) =
                oldest_global_fifo_head_candidate(sessions, &self.cache, None, &skipped_segment_sessions).await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let Some(current_head) = fifo_head_size_candidate(
                &session,
                &protected_set_for_cache_reclamation(&session, &self.cache, None, None),
            ) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            if current_head.proxy_seq != candidate.proxy_seq {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            }
            let Some(deletion) = remove_segment_entry(&mut session, candidate.proxy_seq) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            total_size = total_size.saturating_sub(candidate.content_length);
            deletions
                .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::GlobalSize });
            deletions.persist_pending(report);

            for map_id in unprotected_unreferenced_map_ids(
                &session,
                &protected_set_for_cache_reclamation(&session, &self.cache, None, None),
            ) {
                if !deletions.has_capacity() {
                    break;
                }
                if let Some(deletion) = remove_map_entry(&mut session, map_id) {
                    total_size = total_size.saturating_sub(deletion.content_length);
                    deletions.push(CacheObjectDeletion::Map(deletion.key));
                    deletions.persist_pending(report);
                }
            }
        }
    }

    pub(super) async fn collect_projected_global_reclamation(
        &self,
        sessions: &[HlsSessionHandle],
        excluded_path: &std::path::Path,
        required_bytes: u64,
        max_deletions: usize,
        deletions: &mut CacheDeletionBatch,
    ) {
        let mut planned_bytes = 0_u64;
        let mut planned_deletions = 0_usize;
        let mut transient_readiness_advanced = HashSet::new();
        let mut skipped_transient_sessions = HashSet::new();
        while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
            let Some(candidate) = oldest_global_transient_object_candidate(
                sessions,
                &self.cache,
                Some(excluded_path),
                &skipped_transient_sessions,
            )
            .await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let protected = protected_set_for_cache_reclamation(&session, &self.cache, Some(excluded_path), None);
            let Some(removal) = session.transient.remove_oldest_ready_object_except(&protected.transient_object_ids)
            else {
                skipped_transient_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            planned_bytes = planned_bytes.saturating_add(removal.content_length);
            if transient_readiness_advanced.insert(session.proxy_session_id.clone()) {
                session.advance_media_readiness_generation();
            }
            deletions.push(CacheObjectDeletion::TransientObject {
                key: removal.key,
                content_length: removal.content_length,
            });
            planned_deletions = planned_deletions.saturating_add(1);
        }

        let mut skipped_segment_sessions = HashSet::new();
        while planned_bytes < required_bytes && planned_deletions < max_deletions && deletions.has_capacity() {
            let Some(candidate) = oldest_global_fifo_head_candidate(
                sessions,
                &self.cache,
                Some(excluded_path),
                &skipped_segment_sessions,
            )
            .await
            else {
                break;
            };
            let mut session = candidate.session.write().await;
            let protected = protected_set_for_cache_reclamation(&session, &self.cache, Some(excluded_path), None);
            let Some(current_head) = fifo_head_size_candidate(&session, &protected) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            if current_head.proxy_seq != candidate.proxy_seq {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            }
            let Some(deletion) = remove_segment_entry(&mut session, candidate.proxy_seq) else {
                skipped_segment_sessions.insert(candidate.proxy_session_id);
                continue;
            };
            planned_bytes = planned_bytes.saturating_add(candidate.content_length);
            deletions
                .push(CacheObjectDeletion::Segment { key: deletion, reason: SegmentCacheDeletionReason::GlobalSize });
            planned_deletions = planned_deletions.saturating_add(1);

            for map_id in unprotected_unreferenced_map_ids(
                &session,
                &protected_set_for_cache_reclamation(&session, &self.cache, Some(excluded_path), None),
            ) {
                if planned_bytes >= required_bytes || planned_deletions >= max_deletions || !deletions.has_capacity() {
                    break;
                }
                if let Some(deletion) = remove_map_entry(&mut session, map_id) {
                    planned_bytes = planned_bytes.saturating_add(deletion.content_length);
                    deletions.push(CacheObjectDeletion::Map(deletion.key));
                    planned_deletions = planned_deletions.saturating_add(1);
                }
            }
        }
    }
}
