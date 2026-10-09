use super::{
    safe_proxy_session_id, HlsAccessLease, HlsAccessLeaseDenialMode, HlsAccessLeaseDenialOutcome, HlsAccessLeaseId,
    HlsAccessLeaseLifecycleSnapshot, HlsAccessLeasePendingDeadline, HlsAccessLeaseRemovalPreparation,
    HlsAccessLeaseState, HlsAccessLeaseTiming, HlsCtx, HlsExpiredSessionReason, HlsLifecycleEvent,
    HlsLifecycleEventKey, HlsLifecycleManager, HlsProxyManager, HlsSessionHandle, HlsTerminalTailProtectionRemoval,
    ProxySessionId, HLS_SESSION_IDLE_PROTECTION_RETRY_MS,
};
use log::{debug, error};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tuliprox_core::utils::current_time_millis;
use tuliprox_session::{ActiveProviderManager, ActiveUserManager};

pub(super) fn hls_session_idle_protection_retry_at(due_at_ms: u64, now_ms: u64) -> u64 {
    due_at_ms.max(now_ms.saturating_add(HLS_SESSION_IDLE_PROTECTION_RETRY_MS))
}

impl HlsProxyManager {
    pub fn lifecycle(&self) -> &Arc<HlsLifecycleManager> { &self.lifecycle }

    pub async fn active_live_playback_snapshots_for_session(
        &self,
        proxy_session_id: &ProxySessionId,
        now_ms: u64,
    ) -> Vec<HlsAccessLease> {
        self.access_leases.write().await.active_live_playback_snapshots_for_session(proxy_session_id, now_ms)
    }

    pub async fn touch_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
        timing: HlsAccessLeaseTiming,
    ) -> bool {
        let lease = self.access_leases.write().await.touch_access_lease_snapshot(lease_id, now_ms, timing);
        if let Some(lease) = lease {
            self.schedule_access_lease_activity(&lease).await;
            self.schedule_access_lease_validity(&lease).await;
            true
        } else {
            false
        }
    }

    async fn access_lease_lifecycle_snapshot(
        &self,
        lease_id: &HlsAccessLeaseId,
        now_ms: u64,
    ) -> Option<HlsAccessLeaseLifecycleSnapshot> {
        self.access_leases.write().await.lifecycle_snapshot(lease_id, now_ms)
    }

    pub(super) async fn remove_access_lease(&self, lease_id: &HlsAccessLeaseId) {
        self.terminal_pending.cancel_lease(lease_id);
        self.terminal_commit_retries.cancel_lease(lease_id);
        let Some(preparation) = self.access_leases.read().await.prepare_access_lease_removal(lease_id) else {
            return;
        };
        if !self.remove_prepared_access_lease(lease_id, &preparation).await {
            debug!(
                "HLS access lease removal skipped: lease={} proxy_session={} reason=lease_instance_race",
                super::super::safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(&preparation.proxy_session_id)
            );
        }
    }

    /// Removes one exact lease incarnation before releasing any media
    /// protection owned by it. A stale preparation is intentionally
    /// non-destructive because the replacement lease may reuse the same ID.
    pub(super) async fn remove_prepared_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        preparation: &HlsAccessLeaseRemovalPreparation,
    ) -> bool {
        let removed =
            self.access_leases.write().await.remove_access_lease_if_preparation_matches(lease_id, preparation);
        if removed.is_none() {
            return false;
        }
        if let Some(session) = self.sessions.get_by_proxy_session_id(&preparation.proxy_session_id).await {
            let mut session = session.write().await;
            let finalized_manifest_released =
                session.transient.release_finalized_manifest_generations(lease_id, preparation.issued_at_ms);
            if finalized_manifest_released {
                debug!(
                    "HLS finalized transient manifest generation released: lease={} proxy_session={} reason=lease_removed",
                    super::super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&preparation.proxy_session_id)
                );
            }
            if let Some(generation) = preparation.terminal_protection_generation {
                let removal = session.remove_terminal_tail_protection_after_lease_end(lease_id, generation);
                match removal {
                    HlsTerminalTailProtectionRemoval::Removed => {
                        debug!(
                            "HLS terminal-tail protection released: lease={} proxy_session={} generation={} reason=lease_removed",
                            super::super::safe_hls_access_lease_id(lease_id),
                            safe_proxy_session_id(&preparation.proxy_session_id),
                            generation.0
                        );
                    }
                    HlsTerminalTailProtectionRemoval::Missing => {}
                    HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual } => {
                        debug!(
                            "HLS stale terminal-tail protection released: lease={} proxy_session={} expected_generation={} actual_generation={} reason=lease_removed",
                            super::super::safe_hls_access_lease_id(lease_id),
                            safe_proxy_session_id(&preparation.proxy_session_id),
                            generation.0,
                            actual.0
                        );
                    }
                }
            } else if session.remove_terminal_tail_protection(lease_id).is_some() {
                debug!(
                    "HLS untracked terminal-tail protection released: lease={} proxy_session={} reason=lease_removed",
                    super::super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&preparation.proxy_session_id)
                );
            }
        }
        self.standalone_custom_access.remove(lease_id);
        self.segment_repair.remove_access_lease_window(lease_id).await;
        self.startup_observability.remove_access_lease(lease_id);
        self.qos.remove_access_lease(lease_id).await;
        true
    }

    pub(super) async fn schedule_access_lease_activity(&self, lease: &HlsAccessLease) {
        if let Some(active_until_ms) = lease.active_until_ms {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::AccessLeaseActive {
                        lease_id: lease.lease_id.clone(),
                        proxy_session_id: lease.proxy_session_id.clone(),
                    },
                    active_until_ms,
                )
                .await;
        }
    }

    pub(super) async fn schedule_access_lease_validity(&self, lease: &HlsAccessLease) {
        let due_at_ms = if lease.state == HlsAccessLeaseState::Pending {
            lease.pending_deadline_ms().unwrap_or(lease.valid_until_ms)
        } else {
            lease.valid_until_ms
        };
        self.lifecycle
            .schedule(
                HlsLifecycleEventKey::AccessLeaseValidity {
                    lease_id: lease.lease_id.clone(),
                    proxy_session_id: lease.proxy_session_id.clone(),
                },
                due_at_ms,
            )
            .await;
    }

    async fn schedule_access_lease_lifecycle_snapshot(&self, snapshot: &HlsAccessLeaseLifecycleSnapshot) {
        if snapshot.state == HlsAccessLeaseState::Activated {
            if let Some(active_until_ms) = snapshot.active_until_ms {
                self.lifecycle
                    .schedule(
                        HlsLifecycleEventKey::AccessLeaseActive {
                            lease_id: snapshot.lease_id.clone(),
                            proxy_session_id: snapshot.proxy_session_id.clone(),
                        },
                        active_until_ms,
                    )
                    .await;
            }
        }
        if snapshot.state != HlsAccessLeaseState::Expired && snapshot.state != HlsAccessLeaseState::Denied {
            let due_at_ms = if snapshot.state == HlsAccessLeaseState::Pending {
                snapshot.pending_deadline.map_or(snapshot.valid_until_ms, HlsAccessLeasePendingDeadline::deadline_ms)
            } else {
                snapshot.valid_until_ms
            };
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::AccessLeaseValidity {
                        lease_id: snapshot.lease_id.clone(),
                        proxy_session_id: snapshot.proxy_session_id.clone(),
                    },
                    due_at_ms,
                )
                .await;
        }
    }

    pub async fn schedule_session_idle_for_handle(&self, session: &HlsSessionHandle) {
        let session_idle_timeout_ms = self.session_idle_timeout_ms();
        let (proxy_session_id, due_at_ms) = {
            let session = session.read().await;
            (session.proxy_session_id.clone(), session.idle_expiry_due_at_ms(session_idle_timeout_ms))
        };
        self.lifecycle.schedule(HlsLifecycleEventKey::SessionIdle { proxy_session_id }, due_at_ms).await;
    }

    pub async fn handle_lifecycle_event(
        &self,
        active_users: &Arc<ActiveUserManager>,
        active_provider: &Arc<ActiveProviderManager>,
        event: HlsLifecycleEvent,
        now_ms: u64,
    ) {
        if !self.is_enabled() {
            return;
        }
        match event.key {
            HlsLifecycleEventKey::AccessLeaseActive { lease_id, proxy_session_id }
            | HlsLifecycleEventKey::AccessLeaseValidity { lease_id, proxy_session_id } => {
                let mut should_sync_session = false;
                if let Some(snapshot) = self.access_lease_lifecycle_snapshot(&lease_id, now_ms).await {
                    should_sync_session = true;
                    if let Some(release) = &snapshot.idle_release {
                        active_users
                            .release_session_streams_and_counted_reservation(
                                &release.username,
                                &release.user_session_token,
                            )
                            .await;
                        debug!(
                            "HLS access lease idled: lease={} proxy_session={} user_session={}",
                            super::super::safe_hls_access_lease_id(&release.lease_id),
                            safe_proxy_session_id(&snapshot.proxy_session_id),
                            super::super::safe_user_session_token(&release.user_session_token)
                        );
                    }
                    if matches!(snapshot.state, HlsAccessLeaseState::Expired | HlsAccessLeaseState::Denied) {
                        self.remove_access_lease(&snapshot.lease_id).await;
                        debug!(
                            "HLS access lease removed: lease={} proxy_session={} state={}",
                            super::super::safe_hls_access_lease_id(&snapshot.lease_id),
                            safe_proxy_session_id(&snapshot.proxy_session_id),
                            snapshot.state.as_log_value()
                        );
                        debug!(
                            "HLS lifecycle state snapshot: trigger=access-lease-removed {}",
                            self.debug_state_summary().await
                        );
                    } else {
                        self.schedule_access_lease_lifecycle_snapshot(&snapshot).await;
                    }
                }
                if should_sync_session {
                    if let Some(session) = self.sessions.get_by_proxy_session_id(&proxy_session_id).await {
                        self.sync_session_access_lease_count_and_detach_if_needed(
                            active_users,
                            active_provider,
                            &session,
                            &proxy_session_id,
                            now_ms,
                        )
                        .await;
                    }
                }
            }
            HlsLifecycleEventKey::SessionIdle { proxy_session_id } => {
                self.handle_session_idle_lifecycle_event(&proxy_session_id, now_ms).await;
            }
        }
    }

    async fn handle_session_idle_lifecycle_event(&self, proxy_session_id: &ProxySessionId, now_ms: u64) {
        let Some(session) = self.sessions.get_by_proxy_session_id(proxy_session_id).await else {
            return;
        };
        let session_idle_timeout_ms = self.session_idle_timeout_ms();
        let (key, due_at_ms, can_remove) = {
            let session = session.read().await;
            (
                session.key.clone(),
                session.idle_expiry_due_at_ms(session_idle_timeout_ms),
                session.can_expire_idle_session(now_ms, session_idle_timeout_ms),
            )
        };
        if !can_remove {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    hls_session_idle_protection_retry_at(due_at_ms, now_ms),
                )
                .await;
            return;
        }
        if self.segment_cache.has_active_temp_files_for_session(proxy_session_id) {
            self.lifecycle
                .schedule(
                    HlsLifecycleEventKey::SessionIdle { proxy_session_id: proxy_session_id.clone() },
                    now_ms.saturating_add(1_000),
                )
                .await;
            return;
        }
        let username = self.access_leases.read().await.first_username_for_session(proxy_session_id);
        if self
            .sessions
            .remove_session_marking_expired(
                &key,
                proxy_session_id,
                now_ms,
                HlsExpiredSessionReason::SessionIdleTimeout,
                username,
            )
            .await
            .is_some()
        {
            self.cleanup_proxy_session_state(proxy_session_id, "lifecycle-session-expired").await;
            if let Err(err) = self.segment_cache.delete_session_dir(proxy_session_id).await {
                error!(
                    "HLS session lifecycle cleanup failed: proxy_session={} error={err}",
                    safe_proxy_session_id(proxy_session_id)
                );
            } else {
                debug!("HLS session lifecycle expired: proxy_session={}", safe_proxy_session_id(proxy_session_id));
                debug!("HLS lifecycle state snapshot: trigger=session-expired {}", self.debug_state_summary().await);
            }
        }
    }

    pub async fn deny_access_lease(
        &self,
        lease_id: &HlsAccessLeaseId,
        mode: HlsAccessLeaseDenialMode,
    ) -> HlsAccessLeaseDenialOutcome {
        let outcome = {
            let mut access_leases = self.access_leases.write().await;
            access_leases.deny_access_lease(lease_id, mode)
        };
        let HlsAccessLeaseDenialOutcome::Ended { terminal_release } = &outcome else {
            return outcome;
        };
        self.terminal_pending.cancel_lease(lease_id);
        self.terminal_commit_retries.cancel_lease(lease_id);
        let Some(terminal_release) = terminal_release else {
            return outcome;
        };
        let removal = if let Some(session) =
            self.sessions.get_by_proxy_session_id(&terminal_release.proxy_session_id).await
        {
            session.write().await.remove_terminal_tail_protection_after_lease_end(lease_id, terminal_release.generation)
        } else {
            HlsTerminalTailProtectionRemoval::Missing
        };
        match removal {
            HlsTerminalTailProtectionRemoval::Removed => {
                debug!(
                    "HLS terminal-tail protection released: lease={} proxy_session={} generation={} reason=lease_denied",
                    super::super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&terminal_release.proxy_session_id),
                    terminal_release.generation.0
                );
            }
            HlsTerminalTailProtectionRemoval::Missing => {}
            HlsTerminalTailProtectionRemoval::RemovedStaleGeneration { actual } => {
                debug!(
                    "HLS stale terminal-tail protection released: lease={} proxy_session={} expected_generation={} actual_generation={} reason=lease_denied",
                    super::super::safe_hls_access_lease_id(lease_id),
                    safe_proxy_session_id(&terminal_release.proxy_session_id),
                    terminal_release.generation.0,
                    actual.0
                );
            }
        }
        let acknowledged = self.access_leases.write().await.acknowledge_terminal_protection_release(
            lease_id,
            &terminal_release.proxy_session_id,
            terminal_release.generation,
        );
        if !acknowledged {
            debug!(
                "HLS terminal-tail protection release acknowledgement skipped: lease={} proxy_session={} generation={} reason=lease_generation_race",
                super::super::safe_hls_access_lease_id(lease_id),
                safe_proxy_session_id(&terminal_release.proxy_session_id),
                terminal_release.generation.0
            );
        }
        outcome
    }
}

pub fn exec_hls_lifecycle(ctx: &HlsCtx, cancel_token: &CancellationToken) {
    let hls_proxy = Arc::clone(&ctx.hls_proxy);
    let active_users = Arc::clone(&ctx.active_users);
    let active_provider = Arc::clone(&ctx.active_provider);
    let cancel_token = cancel_token.clone();
    tokio::spawn(async move {
        while let Some(event) = hls_proxy.lifecycle().next_event(&cancel_token).await {
            let now_ms = current_time_millis();
            hls_proxy.handle_lifecycle_event(&active_users, &active_provider, event, now_ms).await;
        }
    });
}
