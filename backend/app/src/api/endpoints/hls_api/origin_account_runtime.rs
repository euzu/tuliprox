#![allow(clippy::wildcard_imports)]
use super::{
    origin_refresh_request::{hls_origin_refresh_request, HlsOriginRefreshOptions},
    session::*,
    *,
};

/// Prepares the origin account runtime for HLS origin work. A healthy known binding is reused without the
/// prepare lock; otherwise the locked path rebinds or acquires an account.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) async fn prepare_hls_origin_runtime(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<PreparedHlsOriginRuntime, HlsOriginRuntimeAcquireError> {
    {
        let current = session.read().await;
        if current.origin_source.input_id != input.id
            || (!current.origin_source.input_name.is_empty() && current.origin_source.input_name != input.name)
            || current.origin_account_binding.as_ref().is_some_and(|binding| binding.input_name != input.name)
        {
            return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
                reason: hls_no_account_reason_for_binding(current.origin_account_binding.as_ref()),
            });
        }
    }
    advance_hls_origin_account_lifecycle(app_state, session, now_ms).await;
    // Healthy preparation does not queue behind account replacement. Preserve the manifest
    // singleflight boundary when many clients arrive while a qualified window awaits media.
    if let Some(current_input) =
        app_state.app_config.get_input_by_id(input.id).filter(|current| current.name == input.name)
    {
        let observed = HlsOriginAccountSnapshot::from_session(&*session.read().await);
        let binding = observed.binding.clone();
        if let Some(binding) = binding.filter(|binding| {
            binding.is_active()
                && hls_origin_account_status(&app_state.hls_ctx(), binding) == HlsOriginAccountStatus::Known
        }) {
            let prepared = prepared_hls_origin_runtime_for_known_binding(
                app_state,
                &current_input,
                raw_request_url,
                session_entry_url,
                &binding,
            )
            .await?;
            let mut current = session.write().await;
            if !current.is_gc_marked_for_removal() && observed.matches_session(&current) {
                if current.origin_account_runtime.is_none() {
                    current.origin_account_runtime = Some(Arc::new(AppHlsOriginAccountRuntime::new(
                        Arc::downgrade(app_state),
                        input.id,
                        Arc::clone(&input.name),
                        raw_request_url.to_string(),
                        session_entry_url.to_string(),
                        fingerprint.clone(),
                    )));
                }
                return Ok(PreparedHlsOriginRuntime::published(
                    prepared,
                    HlsOriginAccountSnapshot::from_session(&current),
                ));
            }
        }
    }
    let prepare_lock = Arc::clone(&session.read().await.origin_account_prepare_lock);
    let _prepare_guard = prepare_lock.lock().await;
    let Some(input) = app_state.app_config.get_input_by_id(input.id).filter(|current| current.name == input.name)
    else {
        let mut session = session.write().await;
        if session.origin_account_rebind.is_allowed_now(now_ms) {
            if let Some(binding) = session.origin_account_binding.clone() {
                session.origin_account_rebind.mark_attempt_started(binding.account_name, now_ms);
                session.origin_account_rebind.mark_failed(now_ms);
            }
        }
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: hls_no_account_reason_for_binding(session.origin_account_binding.as_ref()),
        });
    };
    session.write().await.origin_account_runtime = Some(Arc::new(AppHlsOriginAccountRuntime::new(
        Arc::downgrade(app_state),
        input.id,
        Arc::clone(&input.name),
        raw_request_url.to_string(),
        session_entry_url.to_string(),
        fingerprint.clone(),
    )));
    let input = input.as_ref();
    advance_hls_origin_account_lifecycle(app_state, session, now_ms).await;
    let observed = HlsOriginAccountSnapshot::from_session(&*session.read().await);
    let previous = observed.binding.clone();
    let mut prepared = prepare_hls_origin_runtime_locked(
        app_state,
        session,
        input,
        raw_request_url,
        session_entry_url,
        proxy_session_id,
        fingerprint,
        connection_kind,
        priority,
        work_kind,
        work_class,
        now_ms,
    )
    .await?;
    let snapshot = publish_hls_origin_account_with_status(app_state, session, |current| {
        let snapshot = if let Some(snapshot) = prepared.prepared_snapshot.take() {
            if current.is_gc_marked_for_removal() || !snapshot.matches_session(current) {
                return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
            }
            snapshot
        } else {
            if current.is_gc_marked_for_removal() || !observed.matches_session(current) {
                return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
            }
            if let Some(mut binding) = prepared.binding_to_publish.take() {
                binding.generation = previous.as_ref().map_or(0, |previous| previous.generation.saturating_add(1));
                binding.provider_binding_tag = prepared
                    .preacquired_origin_account_handle
                    .as_ref()
                    .and_then(tuliprox_session::ManagedProviderHandle::handle)
                    .and_then(|handle| handle.binding_tag);
                let admitted_restart = current.origin_playback_kind() == PlaybackKind::LiveHls
                    && prepared
                        .preacquired_origin_account_handle
                        .as_ref()
                        .and_then(tuliprox_session::ManagedProviderHandle::handle)
                        .is_some()
                    && previous.as_ref().is_some_and(|previous| {
                        if !previous.is_detached() {
                            return false;
                        }
                        let failed_start = matches!(
                            previous.binding_mode,
                            HlsOriginAccountBindingMode::Detached {
                                reason: HlsOriginAccountDetachedReason::Cleanup,
                                ..
                            }
                        ) && current
                            .origin_refresh
                            .attempt
                            .as_ref()
                            .is_some_and(|attempt| attempt.failed() && !attempt.active_live_playback);
                        failed_start
                            || matches!(
                                current.account_binding_protection(now_ms),
                                HlsAccountBindingProtection::Expired
                            )
                    });
                if admitted_restart {
                    // A detached start re-entering after expiry has obtained a
                    // real allocation. Its next accepted manifest starts a new
                    // window; polling without admission keeps the original anchor.
                    current.restart_admitted_origin_account(binding);
                    current.origin_control.path_condition = tuliprox_hls::api::HlsOriginPathCondition::ProgressExpected;
                } else if previous.as_ref().is_some_and(|previous| previous.account_name != binding.account_name) {
                    current.handoff_origin_account(binding);
                } else {
                    current.replace_origin_account_binding(Some(binding));
                }
            }
            HlsOriginAccountSnapshot::from_session(current)
        };
        Ok(snapshot)
    })
    .await?;
    Ok(PreparedHlsOriginRuntime::published(prepared, snapshot))
}

// Lock order: viewer rows, then HLS state. Publication and projection are synchronous once
// both locks are acquired: cancellation cannot publish an account without publishing its UI rows.
pub(super) async fn publish_hls_origin_account_with_status(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    publish: impl FnOnce(
        &mut tuliprox_hls::api::HlsSession,
    ) -> Result<HlsOriginAccountSnapshot, HlsOriginRuntimeAcquireError>,
) -> Result<HlsOriginAccountSnapshot, HlsOriginRuntimeAcquireError> {
    let shared_stream_id = hls_cache_shared_stream_id(&session.read().await.proxy_session_id);
    let mut projection = app_state.active_users.lock_shared_stream_provider_projection(shared_stream_id).await;
    let mut current = session.write().await;
    let snapshot = publish(&mut current)?;
    if !snapshot.matches_session(&current) {
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }
    if let Some(binding) = snapshot.binding.as_ref().filter(|binding| binding.is_active()) {
        projection.update_provider(&binding.account_name);
    }
    Ok(snapshot)
}

async fn advance_hls_origin_account_lifecycle(app_state: &Arc<AppState>, session: &HlsSessionHandle, now_ms: u64) {
    promote_elapsed_hls_account_overlaps(app_state, now_ms).await;
    detach_unprotected_hls_origin_account_bindings(app_state, now_ms).await;
    reclaim_hls_account_overlap_if_needed(app_state, session, now_ms).await;
}

/// Under the prepare lock: reuses a known binding, rebinds a stale one, or acquires a new account.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn prepare_hls_origin_runtime_locked(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    proxy_session_id: &ProxySessionId,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    work_kind: HlsOriginWorkKind,
    work_class: HlsOriginWorkClass,
    now_ms: u64,
) -> Result<HlsOriginRuntimeAllocation, HlsOriginRuntimeAcquireError> {
    let playback_kind = hls_session_origin_playback_kind(session).await;
    let existing_binding = session.read().await.origin_account_binding.clone();
    let reacquire_detached_binding = existing_binding.as_ref().is_some_and(HlsOriginAccountBinding::is_detached);
    if reacquire_detached_binding && !session.read().await.origin_account_rebind.is_allowed_now(now_ms) {
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    }
    let final_no_account_reason = hls_no_account_reason_for_binding(existing_binding.as_ref());
    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_started(session, work_kind).await;
    }
    let preferred_account = existing_binding
        .as_ref()
        .filter(|binding| {
            binding.is_detached()
                && hls_origin_account_status(&app_state.hls_ctx(), binding) == HlsOriginAccountStatus::Known
        })
        .map(|binding| Arc::clone(&binding.account_name));
    if let Some(binding) = existing_binding {
        if binding.is_active()
            || !matches!(hls_origin_account_status(&app_state.hls_ctx(), &binding), HlsOriginAccountStatus::Known)
        {
            match hls_origin_account_status(&app_state.hls_ctx(), &binding) {
                stale_status @ (HlsOriginAccountStatus::Missing
                | HlsOriginAccountStatus::Expired
                | HlsOriginAccountStatus::Blocked) => {
                    return rebind_hls_origin_account(
                        app_state,
                        session,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                        stale_status,
                        fingerprint,
                        connection_kind,
                        priority,
                        playback_kind,
                        now_ms,
                    )
                    .await;
                }
                HlsOriginAccountStatus::Known => {
                    return prepared_hls_origin_runtime_for_known_binding(
                        app_state,
                        input,
                        raw_request_url,
                        session_entry_url,
                        &binding,
                    )
                    .await;
                }
            }
        }
    }

    match prepare_hls_origin_runtime_with_new_account(
        app_state,
        input,
        raw_request_url,
        session_entry_url,
        proxy_session_id,
        fingerprint,
        connection_kind,
        priority,
        false,
        work_kind,
        work_class,
        playback_kind,
        preferred_account.as_ref(),
        now_ms,
    )
    .await
    {
        Ok(prepared) => {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.binding_to_publish.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
        Err(HlsOriginRuntimeAcquireError::Fatal(status)) => return Err(HlsOriginRuntimeAcquireError::Fatal(status)),
        Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { .. }) => {}
    }

    if work_class.allows_speculative_overlap() {
        if let Ok(prepared) = prepare_hls_origin_policy_preempt_runtime(
            app_state,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            playback_kind,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.binding_to_publish.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }

        if let Ok(prepared) = prepare_hls_speculative_origin_runtime(
            app_state,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            playback_kind,
            now_ms,
        )
        .await
        {
            if reacquire_detached_binding {
                if let Some(binding) = prepared.binding_to_publish.as_ref() {
                    log_hls_origin_binding_reacquired(session, binding).await;
                }
            }
            return Ok(prepared);
        }
    } else {
        debug!("HLS account overlap skipped: work_class={} reason=background-origin-work", work_class.as_log_value());
    }

    if work_class.allows_grace() {
        match prepare_hls_origin_runtime_with_new_account(
            app_state,
            input,
            raw_request_url,
            session_entry_url,
            proxy_session_id,
            fingerprint,
            connection_kind,
            priority,
            true,
            work_kind,
            work_class,
            playback_kind,
            None,
            now_ms,
        )
        .await
        {
            Ok(prepared) => {
                if reacquire_detached_binding {
                    if let Some(binding) = prepared.binding_to_publish.as_ref() {
                        log_hls_origin_binding_reacquired(session, binding).await;
                    }
                }
                return Ok(prepared);
            }
            Err(HlsOriginRuntimeAcquireError::Fatal(status)) => {
                return Err(HlsOriginRuntimeAcquireError::Fatal(status))
            }
            Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { .. }) => {}
        }
    } else {
        debug!(
            "HLS origin account grace skipped: work_class={} reason=background-origin-work",
            work_class.as_log_value()
        );
    }

    if reacquire_detached_binding {
        log_hls_origin_binding_reacquire_failed(session, "no-account-available").await;
    }
    Err(HlsOriginRuntimeAcquireError::NoAccountAvailable { reason: final_no_account_reason })
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) async fn rebind_hls_origin_account(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    raw_request_url: &str,
    session_entry_url: &str,
    stale_binding: &HlsOriginAccountBinding,
    stale_status: HlsOriginAccountStatus,
    fingerprint: &Fingerprint,
    connection_kind: crate::api::model::ConnectionKind,
    priority: i8,
    playback_kind: PlaybackKind,
    now_ms: u64,
) -> Result<HlsOriginRuntimeAllocation, HlsOriginRuntimeAcquireError> {
    {
        let mut session_guard = session.write().await;
        if !session_guard.origin_account_rebind.is_allowed_now(now_ms) {
            debug!(
                "HLS origin account rebind skipped by backoff: proxy_session={} old_account={} retry_after_ms=2000",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref())
            );
            return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
        }
        session_guard.origin_account_rebind.mark_attempt_started(Arc::clone(&stale_binding.account_name), now_ms);
    }

    let safe_proxy_session = {
        let session_guard = session.read().await;
        safe_proxy_session_id(&session_guard.proxy_session_id)
    };
    debug!(
        "HLS origin account rebind started: proxy_session={} old_account={} reason={stale_status:?}",
        safe_proxy_session,
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
    app_state.active_provider.clear_identified_provider_reservation(
        &stale_binding.session_owner,
        &stale_binding.account_name,
        stale_binding.provider_binding_tag,
    );
    let expected_work_generation = {
        let mut session_guard = session.write().await;
        if let Some(binding) = session_guard.origin_account_binding.as_mut().filter(|binding| {
            binding.account_name == stale_binding.account_name
                && binding.session_owner == stale_binding.session_owner
                && binding.provider_binding_tag == stale_binding.provider_binding_tag
        }) {
            binding.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, now_ms);
            debug!(
                "HLS origin binding detached: proxy_session={} account={} reason={}",
                safe_proxy_session_id(&session_guard.proxy_session_id),
                sanitize_sensitive_info(stale_binding.account_name.as_ref()),
                HlsOriginAccountDetachedReason::AccountMissingOrExpired.as_log_reason()
            );
        }
        session_guard.invalidate_queued_origin_work();
        session_guard.activity.origin_work_generation
    };

    let Some(provider_handle) = app_state.active_provider.acquire_connection_with_lease_for_session(
        &input.name,
        &fingerprint.addr,
        false,
        priority,
        connection_kind,
        Some(PlaybackLeaseRef::new(&stale_binding.session_owner, playback_kind)),
    ) else {
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_account_available").await;
        return Err(HlsOriginRuntimeAcquireError::NoAccountAvailable {
            reason: hls_no_account_reason_for_binding(Some(stale_binding)),
        });
    };

    let managed_handle =
        tuliprox_session::ManagedProviderHandle::new(Arc::clone(&app_state.active_provider), provider_handle);
    let Some(provider_config) = managed_handle.handle().and_then(|handle| handle.allocation.get_provider_config())
    else {
        drop(managed_handle);
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "no_provider_config").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(fetch_url) = resolve_hls_origin_fetch_url(
        &app_state.app_config,
        input,
        raw_request_url,
        session_entry_url,
        Some(&provider_config),
    )
    .await
    else {
        drop(managed_handle);
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "invalid_origin_url").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let Some(new_account_name) = managed_handle.handle().and_then(|handle| handle.allocation.get_provider_name())
    else {
        drop(managed_handle);
        mark_hls_origin_rebind_failed(session, stale_binding, now_ms, "missing_account_name").await;
        return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
    };

    let mut new_binding = HlsOriginAccountBinding::rebound(
        Arc::clone(&input.name),
        new_account_name,
        stale_binding.session_owner.clone(),
        stale_binding.generation.saturating_add(1),
        now_ms,
    );
    new_binding.provider_binding_tag = managed_handle.handle().and_then(|handle| handle.binding_tag);
    let prepared_snapshot = publish_hls_origin_account_with_status(app_state, session, |session_guard| {
        if !publish_hls_origin_account_rebind(
            session_guard,
            stale_binding,
            expected_work_generation,
            new_binding.clone(),
        ) {
            return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
        }
        Ok(HlsOriginAccountSnapshot::from_session(session_guard))
    })
    .await?;
    debug!(
        "HLS origin account rebound: old_account={} new_account={}",
        sanitize_sensitive_info(stale_binding.account_name.as_ref()),
        sanitize_sensitive_info(new_binding.account_name.as_ref())
    );

    Ok(HlsOriginRuntimeAllocation {
        url_failover_provider: hls_url_failover_provider_for_origin_context(
            input,
            raw_request_url,
            session_entry_url,
            &fetch_url,
        ),
        runtime_provider_config: Some(provider_config),
        fetch_url,
        binding_to_publish: None,
        prepared_snapshot: Some(prepared_snapshot),
        preacquired_origin_account_handle: Some(managed_handle),
    })
}

// Resolution may suspend after detaching. Publish only into the same account and work generation.
fn publish_hls_origin_account_rebind(
    session: &mut tuliprox_hls::api::HlsSession,
    expected: &HlsOriginAccountBinding,
    expected_work_generation: u64,
    replacement: HlsOriginAccountBinding,
) -> bool {
    if session.is_gc_marked_for_removal()
        || session.activity.origin_work_generation != expected_work_generation
        || !session.origin_account_binding.as_ref().is_some_and(|current| {
            current.input_name == expected.input_name
                && current.account_name == expected.account_name
                && current.session_owner == expected.session_owner
                && current.generation == expected.generation
                && current.provider_binding_tag == expected.provider_binding_tag
        })
    {
        return false;
    }
    session.handoff_origin_account(replacement);
    session.origin_account_rebind.mark_success();
    true
}

pub(super) async fn mark_hls_origin_rebind_failed(
    session: &HlsSessionHandle,
    stale_binding: &HlsOriginAccountBinding,
    now_ms: u64,
    reason: &str,
) {
    let mut session_guard = session.write().await;
    session_guard.origin_account_rebind.mark_failed(now_ms);
    debug!(
        "HLS origin account rebind failed: proxy_session={} old_account={} reason={reason} retry_after_ms=2000",
        safe_proxy_session_id(&session_guard.proxy_session_id),
        sanitize_sensitive_info(stale_binding.account_name.as_ref())
    );
}

use crate::api::api_utils::{
    failover_group::{group_playback_settings, record_member_failure, GroupSelectionRequest, SelectionMode},
    failover_group_runtime::{BudgetDecision, FailoverBudget},
};
use arc_swap::ArcSwap;
use shared::model::{FailoverGroupRuntimeConfig, VirtualId};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::Instant,
};
use tuliprox_hls::api::{
    build_hls_origin_session_owner, hls_manifest_acceptance_directive_for_session, trigger_origin_refresh_sync,
    HlsBoundAccountAcquireErrorKind, HlsManifestAcceptanceDirective, HlsManifestAcceptanceEvaluationOutcome,
    HlsMemberHealthPolicy, HlsMemberUnhealthyReason, HlsOriginAccountRuntime, HlsOriginAccountSnapshot,
    HlsOriginAccountWork, HlsOriginSource, HlsPreparedOriginAccount, HlsSession, LiveHlsOriginEntry,
};
use tuliprox_repository::target_stream_item::{classify_virtual_id, ResolvedStreamTarget};

/// Swappable member identity installed on the app origin-account runtime. A group member handoff
/// replaces this binding atomically; single-item sessions keep the value installed at creation.
#[derive(Clone)]
struct AppHlsMemberBinding {
    input_id: u16,
    input_name: Arc<str>,
    raw_request_url: String,
    entry_url: String,
}

struct AppHlsOriginAccountRuntime {
    app_state: Weak<AppState>,
    member: Arc<ArcSwap<AppHlsMemberBinding>>,
    fingerprint: Fingerprint,
    /// One handoff per session at a time; concurrent notifications coalesce.
    handoff_in_flight: Arc<AtomicBool>,
    /// Incident budget created lazily on the first member-handoff trigger.
    handoff_budget: Arc<tokio::sync::Mutex<Option<FailoverBudget>>>,
    /// Group failover config cached for the lifetime of this runtime (see [`Self::group_failover_config`]).
    cached_failover: std::sync::OnceLock<Option<FailoverGroupRuntimeConfig>>,
}

/// Resets the single-flight handoff flag on every exit, including a panic in the spawned task.
struct HandoffInFlightGuard {
    flag: Arc<AtomicBool>,
}

impl Drop for HandoffInFlightGuard {
    fn drop(&mut self) { self.flag.store(false, Ordering::Release); }
}

impl AppHlsOriginAccountRuntime {
    fn new(
        app_state: Weak<AppState>,
        input_id: u16,
        input_name: Arc<str>,
        raw_request_url: String,
        entry_url: String,
        fingerprint: Fingerprint,
    ) -> Self {
        Self {
            app_state,
            member: Arc::new(ArcSwap::from_pointee(AppHlsMemberBinding {
                input_id,
                input_name,
                raw_request_url,
                entry_url,
            })),
            fingerprint,
            handoff_in_flight: Arc::new(AtomicBool::new(false)),
            handoff_budget: Arc::new(tokio::sync::Mutex::new(None)),
            cached_failover: std::sync::OnceLock::new(),
        }
    }
}

impl AppHlsOriginAccountRuntime {
    /// Runtime failover settings of the group the session belongs to; `None` for single-item sessions.
    fn group_failover_config(&self, session: &HlsSession) -> Option<FailoverGroupRuntimeConfig> {
        // Computed once for this runtime. A live session keeps its runtime across member handoffs,
        // so a config reload is not observed for the rest of the session; that is acceptable because
        // these values are timing thresholds, not routing decisions.
        *self.cached_failover.get_or_init(|| {
            let (target_id, _) = session.key.group_identity()?;
            let app_state = self.app_state.upgrade()?;
            let target = app_state.app_config.get_target_by_id(target_id)?;
            Some(group_playback_settings(&app_state, &target).failover)
        })
    }
}

impl std::fmt::Debug for AppHlsOriginAccountRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppHlsOriginAccountRuntime")
            .field("input_id", &self.member.load().input_id)
            .finish_non_exhaustive()
    }
}

impl HlsOriginAccountRuntime for AppHlsOriginAccountRuntime {
    fn raw_request_url(&self) -> Option<String> { Some(self.member.load().raw_request_url.clone()) }

    fn entry_url(&self) -> Option<String> { Some(self.member.load().entry_url.clone()) }

    fn member_health_policy(&self, session: &HlsSession) -> Option<HlsMemberHealthPolicy> {
        let failover = self.group_failover_config(session)?;
        failover.enabled.then(|| HlsMemberHealthPolicy {
            no_data_timeout_ms: failover.no_data_timeout_secs.saturating_mul(1_000),
            slow_source_pct: failover.slow_source_pct,
            slow_source_window_ms: failover.slow_source_secs.saturating_mul(1_000),
        })
    }

    fn notify_member_progress(&self, session: &HlsSession) {
        // Never wait for a handoff that currently holds the budget; the next segment reports again.
        let Ok(mut budget) = self.handoff_budget.try_lock() else {
            return;
        };
        if budget.is_none() {
            *budget = self.group_failover_config(session).map(|failover| FailoverBudget::new(&failover));
        }
        if let Some(budget) = budget.as_mut() {
            budget.on_data(Instant::now());
        }
    }

    fn notify_member_unhealthy(&self, session: &HlsSessionHandle, reason: HlsMemberUnhealthyReason) {
        let app_state = self.app_state.clone();
        let member = Arc::clone(&self.member);
        let handoff_in_flight = Arc::clone(&self.handoff_in_flight);
        let handoff_budget = Arc::clone(&self.handoff_budget);
        let fingerprint = self.fingerprint.clone();
        let session = Arc::clone(session);
        if handoff_in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        tokio::spawn(async move {
            let _guard = HandoffInFlightGuard { flag: handoff_in_flight };
            let Some(app_state) = app_state.upgrade() else { return };
            handoff_hls_group_member(&app_state, &session, &handoff_budget, &member, &fingerprint, reason).await;
        });
    }

    #[allow(clippy::too_many_lines)]
    fn prepare<'a>(
        &'a self,
        session: &'a HlsSessionHandle,
        work: HlsOriginAccountWork,
        class: HlsOriginWorkClass,
    ) -> Pin<Box<dyn Future<Output = Result<HlsPreparedOriginAccount, HlsBoundAccountAcquireErrorKind>> + Send + 'a>>
    {
        Box::pin(async move {
            let app_state = self.app_state.upgrade().ok_or(HlsBoundAccountAcquireErrorKind::Unavailable)?;
            let member = self.member.load();
            let input = app_state
                .app_config
                .get_input_by_id(member.input_id)
                .filter(|input| input.name == member.input_name)
                .ok_or(HlsBoundAccountAcquireErrorKind::Missing)?;
            let (proxy_id, policy, unavailable) = {
                let session = session.read().await;
                (
                    session.proxy_session_id.clone(),
                    session.effective_origin_acquire_policy_or_default(),
                    session.is_gc_marked_for_removal(),
                )
            };
            if unavailable
                || !app_state
                    .hls
                    .proxy
                    .segment_worker_pool()
                    .access_leases()
                    .write()
                    .await
                    .has_usable_access_lease_for_session(&proxy_id, current_time_millis())
            {
                return Err(HlsBoundAccountAcquireErrorKind::Unavailable);
            }
            let prepared = prepare_hls_origin_runtime(
                &app_state,
                session,
                &input,
                &member.raw_request_url,
                &member.entry_url,
                &proxy_id,
                &self.fingerprint,
                policy.connection_kind,
                policy.priority,
                if work == HlsOriginAccountWork::Manifest {
                    HlsOriginWorkKind::Manifest
                } else {
                    HlsOriginWorkKind::Resource
                },
                class,
                current_time_millis(),
            )
            .await
            .map_err(|error| match error {
                HlsOriginRuntimeAcquireError::NoAccountAvailable {
                    reason: HlsOriginRuntimeNoAccountReason::ProviderConnectionsExhausted,
                } => HlsBoundAccountAcquireErrorKind::Exhausted,
                HlsOriginRuntimeAcquireError::NoAccountAvailable {
                    reason: HlsOriginRuntimeNoAccountReason::OriginBindingPreempted,
                } => HlsBoundAccountAcquireErrorKind::Detached,
                HlsOriginRuntimeAcquireError::Fatal(_) => HlsBoundAccountAcquireErrorKind::Unavailable,
            })?;
            // Drop releases unused allocations synchronously, including cancellation after prepare.
            let managed = prepared.preacquired_origin_account_handle;
            let entry = LiveHlsOriginEntry::parse_with_provider_configs(
                &prepared.fetch_url,
                prepared.url_failover_provider,
                prepared.runtime_provider_config,
            )
            .ok_or(HlsBoundAccountAcquireErrorKind::Unavailable)?;
            let snapshot = prepared.snapshot;
            let (pending, origin_headers, reservation_ttl_secs, playback_kind, account_runtime) = {
                let current = session.read().await;
                if current.is_gc_marked_for_removal() || !snapshot.matches_session(&current) {
                    return Err(HlsBoundAccountAcquireErrorKind::StoreRace);
                }
                (
                    current.origin_account_manifest_pending.is_some(),
                    current.origin_request_headers.clone(),
                    current.account_overlap_timing().reservation_ttl_secs(),
                    current.origin_playback_kind(),
                    current.origin_account_runtime.clone(),
                )
            };
            drop(managed);
            if work == HlsOriginAccountWork::Media && pending {
                let origin_io = HlsOriginIoContext {
                    ctx: app_state.hls_ctx(),
                    client_addr: self.fingerprint.addr,
                    allow_grace: class.allows_grace(),
                    priority: policy.priority,
                    connection_kind: policy.connection_kind,
                    reservation_ttl_secs,
                    preacquired_provider_handle: None,
                    started_generation: None,
                    prepared_account: None,
                    playback_kind,
                    account_runtime,
                };
                let manifest_commit_requirement = tuliprox_hls::api::hls_manifest_commit_requirement(
                    session,
                    tuliprox_hls::api::HlsSessionStoreOutcome::Reused,
                    None,
                    false,
                    current_time_millis(),
                )
                .await;
                let request = hls_origin_refresh_request(
                    &app_state,
                    session,
                    HlsPreparedOriginAccount { entry: entry.clone(), snapshot: snapshot.clone() },
                    HlsOriginRefreshOptions {
                        headers: origin_headers,
                        origin_io,
                        manifest_commit_requirement,
                        acceptance_directive: HlsManifestAcceptanceDirective::none(),
                        access_lease_id: None,
                        rewrite_secret: app_state.get_encrypt_secret().to_vec(),
                        now_ms: current_time_millis(),
                    },
                );
                trigger_origin_refresh_sync(request).await;
                if session.read().await.origin_account_manifest_pending.is_some() {
                    return Err(HlsBoundAccountAcquireErrorKind::Unavailable);
                }
            }
            Ok(HlsPreparedOriginAccount { entry, snapshot })
        })
    }
}

fn hls_member_binding_matches(current: &HlsOriginAccountBinding, expected: &HlsOriginAccountBinding) -> bool {
    current.account_name == expected.account_name
        && current.session_owner == expected.session_owner
        && current.generation == expected.generation
        && current.provider_binding_tag == expected.provider_binding_tag
}

/// Clears the old member's reservation exactly once and fences concurrent origin work before a
/// member handoff: detach the old binding and bump the work generation. Returns the fence value.
async fn begin_hls_group_member_handoff(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    old_binding: Option<&HlsOriginAccountBinding>,
    now_ms: u64,
) -> u64 {
    if let Some(binding) = old_binding {
        app_state.active_provider.clear_identified_provider_reservation(
            &binding.session_owner,
            &binding.account_name,
            binding.provider_binding_tag,
        );
    }
    let mut session = session.write().await;
    if let Some(binding) = old_binding {
        if let Some(current) =
            session.origin_account_binding.as_mut().filter(|current| hls_member_binding_matches(current, binding))
        {
            current.detach(HlsOriginAccountDetachedReason::AccountMissingOrExpired, now_ms);
        }
    }
    session.invalidate_queued_origin_work();
    session.activity.origin_work_generation
}

/// Publishes a member handoff only into the same binding and work generation observed before the
/// switch, mirroring the account-rebind publication fence.
fn publish_hls_origin_member_handoff(
    session: &mut tuliprox_hls::api::HlsSession,
    expected: Option<&HlsOriginAccountBinding>,
    expected_work_generation: u64,
    source: HlsOriginSource,
    replacement: HlsOriginAccountBinding,
) -> bool {
    if session.is_gc_marked_for_removal() || session.activity.origin_work_generation != expected_work_generation {
        return false;
    }
    let matches = match (expected, session.origin_account_binding.as_ref()) {
        (Some(expected), Some(current)) => hls_member_binding_matches(current, expected),
        (None, None) => true,
        _ => false,
    };
    if !matches {
        return false;
    }
    session.handoff_origin_member(source, replacement);
    true
}

/// Fetches and commits the new member's manifest through the regular refresh/acceptance pipeline,
/// mirroring the account-rebind follow-up. This makes the host switch, epoch, discontinuity, key
/// namespace and init-map transitions happen inside the handoff rather than on the next poll.
#[allow(clippy::too_many_arguments)]
async fn trigger_hls_group_member_handoff_refresh(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    member_url: &str,
    url_failover_provider: Option<Arc<ConfigProvider>>,
    runtime_provider_config: Option<Arc<crate::model::ProviderConfig>>,
    snapshot: HlsOriginAccountSnapshot,
    fingerprint: &Fingerprint,
    preacquired_handle: tuliprox_session::ManagedProviderHandle,
    now_ms: u64,
) {
    let Some(entry) =
        LiveHlsOriginEntry::parse_with_provider_configs(member_url, url_failover_provider, runtime_provider_config)
    else {
        return;
    };
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let acceptance_directive =
        match hls_manifest_acceptance_directive_for_session(&app_state.hls_ctx(), session, &proxy_session_id).await {
            HlsManifestAcceptanceEvaluationOutcome::Evaluated(directive) => directive,
            HlsManifestAcceptanceEvaluationOutcome::SessionSuperseded => return,
            HlsManifestAcceptanceEvaluationOutcome::StateContention { .. } => HlsManifestAcceptanceDirective::none(),
        };
    let (policy, reservation_ttl_secs, playback_kind, account_runtime, headers) = {
        let session = session.read().await;
        (
            session.effective_origin_acquire_policy_or_default(),
            session.account_overlap_timing().reservation_ttl_secs(),
            session.origin_playback_kind(),
            session.origin_account_runtime.clone(),
            session.origin_request_headers.clone(),
        )
    };
    let origin_io = HlsOriginIoContext {
        ctx: app_state.hls_ctx(),
        client_addr: fingerprint.addr,
        allow_grace: HlsOriginWorkClass::ManifestInteractive.allows_grace(),
        priority: policy.priority,
        connection_kind: policy.connection_kind,
        reservation_ttl_secs,
        preacquired_provider_handle: None,
        started_generation: None,
        prepared_account: None,
        playback_kind,
        account_runtime,
    }
    .with_managed_provider_handle(preacquired_handle);
    let manifest_commit_requirement = tuliprox_hls::api::hls_manifest_commit_requirement(
        session,
        tuliprox_hls::api::HlsSessionStoreOutcome::Reused,
        None,
        false,
        now_ms,
    )
    .await;
    let request = hls_origin_refresh_request(
        app_state,
        session,
        HlsPreparedOriginAccount { entry, snapshot },
        HlsOriginRefreshOptions {
            headers,
            origin_io,
            manifest_commit_requirement,
            acceptance_directive,
            access_lease_id: None,
            rewrite_secret: app_state.get_encrypt_secret().to_vec(),
            now_ms,
        },
    );
    trigger_origin_refresh_sync(request).await;
}

/// Hands a live failover-group session over to the next healthy member. R3 triggers call this via
/// [`HlsOriginAccountRuntime::notify_member_unhealthy`]; R2 also calls it from the cached entry
/// path when a fresh selection disagrees with a non-reusable bound session.
#[allow(clippy::too_many_lines)]
async fn handoff_hls_group_member(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    handoff_budget: &tokio::sync::Mutex<Option<FailoverBudget>>,
    member: &ArcSwap<AppHlsMemberBinding>,
    fingerprint: &Fingerprint,
    reason: HlsMemberUnhealthyReason,
) {
    let now_ms = current_time_millis();
    let Some((target_id, group_vid)) = session.read().await.key.group_identity() else {
        return;
    };
    let Some(target) = app_state.app_config.get_target_by_id(target_id) else {
        return;
    };
    let ResolvedStreamTarget::Group(group) =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, &target, VirtualId::new(group_vid)).await
    else {
        return;
    };

    let (current_index, old_binding, proxy_session_id, playback_kind, policy, request_headers) = {
        let session = session.read().await;
        let input_name = session.origin_source.input_name.clone();
        let stream_ref = session.origin_source.stream_ref.clone();
        let index = group.members.iter().position(|member| {
            member.input_name.as_ref() == input_name.as_ref() && member.input_stream_id.as_ref() == stream_ref.as_str()
        });
        (
            index,
            session.origin_account_binding.clone(),
            session.proxy_session_id.clone(),
            session.origin_playback_kind(),
            session.effective_origin_acquire_policy_or_default(),
            session.origin_request_headers.clone(),
        )
    };

    if let Some(index) = current_index {
        record_member_failure(&app_state.member_health, &group.members[index], None, reason.failure_class());
    }

    let playback = group_playback_settings(app_state, &target);
    let member_count = group.members.len();
    let decision = {
        let mut guard = handoff_budget.lock().await;
        let budget = guard.get_or_insert_with(|| FailoverBudget::new(&playback.failover));
        budget.on_trigger(current_index.unwrap_or(0), member_count, Instant::now())
    };

    let exclude = match decision {
        BudgetDecision::GiveUp => {
            begin_hls_group_member_handoff(app_state, session, old_binding.as_ref(), now_ms).await;
            return;
        }
        BudgetDecision::RetrySame => return,
        BudgetDecision::Next { exclude } => exclude,
    };

    // Serialize the member handoff with concurrent account prepares. Once the old binding is
    // detached below, a client manifest poll can reacquire it through the ordinary prepare path,
    // bump the work generation and invalidate this handoff's publication fence. The follow-up
    // refresh re-enters prepare, so the lock is dropped before it runs.
    let prepare_lock = Arc::clone(&session.read().await.origin_account_prepare_lock);
    let prepare_guard = prepare_lock.lock().await;

    let session_owner = old_binding
        .as_ref()
        .map_or_else(|| build_hls_origin_session_owner(&proxy_session_id), |binding| binding.session_owner.clone());
    let expected_work_generation =
        begin_hls_group_member_handoff(app_state, session, old_binding.as_ref(), now_ms).await;

    let req = GroupSelectionRequest {
        mode: SelectionMode::Live,
        prefer: None,
        lease: Some(PlaybackLeaseRef::new(&session_owner, playback_kind)),
        allow_provider_grace: playback.allow_provider_grace,
        bypass_maintenance: false,
        exclude: &exclude,
        demoted_inputs: &[],
        allow_preempt: false,
        ..GroupSelectionRequest::new(
            target.name.as_str(),
            &group,
            playback.start,
            &fingerprint.addr,
            "",
            policy.priority,
            policy.connection_kind,
        )
    };

    let selected = resolve_checked_hls_group_entry(
        app_state,
        &target,
        Arc::clone(&group),
        req,
        &request_headers,
        true,
        None,
        false,
        fingerprint,
    )
    .await;
    let Ok((resolved, probed)) = selected else {
        // No member left: the old binding is already detached and fenced, so the session's
        // existing terminal/unavailable handling takes over.
        return;
    };

    let Some(provider_config) =
        resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_config())
    else {
        drop(resolved.acquired.handle);
        return;
    };
    let Some(new_account) = resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_name())
    else {
        drop(resolved.acquired.handle);
        return;
    };
    let member_url = if let Some(probed) = probed.as_ref() {
        Some(probed.request_url.clone())
    } else {
        build_hls_group_member_origin_url(
            app_state,
            &target,
            &resolved.input,
            &resolved.member,
            &new_account,
            Some(&provider_config),
        )
        .await
    };
    let Some(member_url) = member_url else {
        drop(resolved.acquired.handle);
        return;
    };

    let (raw_request_url, entry_url) = match build_hls_origin_resolution(&resolved.input, &member_url) {
        Some(resolution) => (resolution.hls_url, resolution.session_entry_url.as_str().to_string()),
        None => (member_url.clone(), member_url.clone()),
    };
    let url_failover_provider =
        hls_url_failover_provider_for_origin_context(&resolved.input, &raw_request_url, &entry_url, &member_url);
    let new_source = build_hls_origin_source_for_playback(
        &resolved.input,
        resolved.member.input_stream_id.to_string(),
        None,
        Some(&member_url),
    );
    let generation = old_binding.as_ref().map_or(0, |binding| binding.generation.saturating_add(1));
    let mut new_binding = HlsOriginAccountBinding::rebound(
        Arc::clone(&resolved.input.name),
        Arc::clone(&new_account),
        session_owner.clone(),
        generation,
        now_ms,
    );
    new_binding.provider_binding_tag = resolved.acquired.handle.handle().and_then(|handle| handle.binding_tag);

    let new_index = resolved.acquired.index;
    let published = publish_hls_origin_account_with_status(app_state, session, |current| {
        if !publish_hls_origin_member_handoff(
            current,
            old_binding.as_ref(),
            expected_work_generation,
            new_source.clone(),
            new_binding.clone(),
        ) {
            return Err(HlsOriginRuntimeAcquireError::Fatal(StatusCode::SERVICE_UNAVAILABLE));
        }
        Ok(HlsOriginAccountSnapshot::from_session(current))
    })
    .await;
    let Ok(snapshot) = published else {
        drop(resolved.acquired.handle);
        return;
    };

    drop(prepare_guard);
    member.store(Arc::new(AppHlsMemberBinding {
        input_id: resolved.input.id,
        input_name: Arc::clone(&resolved.input.name),
        raw_request_url,
        entry_url,
    }));
    let preacquired_handle = resolved.acquired.handle;
    trigger_hls_group_member_handoff_refresh(
        app_state,
        session,
        &member_url,
        url_failover_provider,
        Some(provider_config),
        snapshot,
        fingerprint,
        preacquired_handle,
        now_ms,
    )
    .await;

    {
        let mut guard = handoff_budget.lock().await;
        if let Some(budget) = guard.as_mut() {
            budget.on_switched(new_index, current_index.unwrap_or(new_index), Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuliprox_hls::api::{HlsSession, HlsSessionKey};

    #[test]
    fn rebind_publication_rejects_superseded_work_binding_and_gc() {
        let mut session = HlsSession::new(HlsSessionKey::new(1, "publication"), b"secret", 0);
        let expected =
            HlsOriginAccountBinding::new(Arc::from("input"), Arc::from("expired"), &session.proxy_session_id, 0);
        session.replace_origin_account_binding(Some(expected.clone()));
        let replacement = HlsOriginAccountBinding::rebound(
            Arc::from("input"),
            Arc::from("replacement"),
            expected.session_owner.clone(),
            1,
            1,
        );
        session.invalidate_queued_origin_work();
        assert!(!publish_hls_origin_account_rebind(&mut session, &expected, 0, replacement.clone()));
        assert_eq!(session.origin_account_binding.as_ref().unwrap().account_name, expected.account_name);
        assert!(session.origin_account_manifest_pending.is_none());
        session.origin_account_binding.as_mut().unwrap().generation = 7;
        assert!(!publish_hls_origin_account_rebind(&mut session, &expected, 1, replacement.clone()));
        assert_eq!(session.origin_account_binding.as_ref().unwrap().generation, 7);
        session.replace_origin_account_binding(Some(expected.clone()));
        session.mark_for_gc_removal();
        assert!(!publish_hls_origin_account_rebind(&mut session, &expected, 1, replacement.clone()));
        session.clear_gc_removal_mark();
        assert!(publish_hls_origin_account_rebind(&mut session, &expected, 1, replacement));
        assert_eq!(session.origin_account_binding.as_ref().unwrap().account_name.as_ref(), "replacement");
        assert_eq!(session.activity.origin_work_generation, 2);
        assert_eq!(session.origin_account_manifest_pending, Some(1));
    }
}
