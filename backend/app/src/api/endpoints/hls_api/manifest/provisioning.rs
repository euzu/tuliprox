use super::{
    build_hls_origin_resolution, build_hls_origin_source, get_stream_channel, hls_cache_configured,
    hls_cache_enabled_for_user, hls_canonical_retry_after_response, hls_origin_account_reservation_ttl_secs_fallback,
    hls_origin_account_reservation_ttl_secs_for_session, hls_runtime_or_standalone_custom_tail_response,
    resolve_hls_origin_playlist_url, try_hls_cached_manifest_response, HlsEntryStreamIdentity,
    HlsRuntimeBandwidthLearningContext,
};
use crate::{
    api::{
        api_utils::{
            connection_priority_for_kind, create_playback_session_fingerprint, get_hls_session_ttl_secs,
            resolve_playback_request_admission, select_provider_stream_url, EvictionReentryGuard,
        },
        model::{
            hls_custom_video_manifest_response_with_virtual_id, hls_provisioning_discontinuity_sequence,
            hls_virtual_entry_redirect_response, is_custom_video_stream_enabled, start_hls_panel_provisioning_once,
            try_hls_panel_provisioning_manifest_response, AppState, CustomVideoStreamType,
            HlsPanelProvisioningRedirectPaths, HlsProvisioningStatus, PlaybackLeaseRef,
            ProviderConfig as RuntimeProviderConfig, ProviderHandle, TransportStreamBuffer,
        },
        panel_api::can_provision_on_exhausted,
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, PlaybackKind, ProxyUserCredentials},
};
use axum::{http::StatusCode, response::IntoResponse};
use log::{debug, warn};
use shared::model::{PlaylistItemType, UserConnectionPermission, VirtualId};
use std::{sync::Arc, time::Duration};
use tuliprox_core::utils::current_time_millis;
use tuliprox_hls::api::{
    build_hls_origin_session_owner, build_proxy_session_id, is_hls_provisioning_gap_segment,
    is_hls_provisioning_segment, safe_hls_access_lease_id, safe_proxy_session_id, CacheAccessState, HlsAccessLeaseId,
    HlsAccessLeaseState, HlsCachedManifestOptions, HlsRuntimeCustomTailReason, HlsSession, HlsSessionHandle,
    OriginSegmentKey, SegmentCacheKey, SegmentCacheStatus, SegmentEntry, HLS_PROVISIONING_GAP_ORIGIN_EPOCH,
    HLS_PROVISIONING_ORIGIN_EPOCH, HLS_PROVISIONING_SEGMENT_DURATION_MS, HLS_PROVISIONING_TARGET_DURATION_SECS,
};

pub(in crate::api::endpoints::hls_api) struct HlsEntryOriginAccountReservation {
    pub(in crate::api::endpoints::hls_api) request_url: String,
    pub(in crate::api::endpoints::hls_api) session_token: String,
    pub(in crate::api::endpoints::hls_api) provider_handle: Option<ProviderHandle>,
    pub(in crate::api::endpoints::hls_api) selected_provider_config: Option<Arc<RuntimeProviderConfig>>,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn try_reserve_hls_entry_origin_account_for_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    request_url: &str,
    user_session_token: &str,
    session_owner: &str,
    playback_kind: PlaybackKind,
    reservation_ttl_secs: u64,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    create_user_session: bool,
) -> Option<HlsEntryOriginAccountReservation> {
    let provider_handle = app_state.active_provider.acquire_connection_with_lease_for_session(
        &input.name,
        &fingerprint.addr,
        false,
        connection_priority_for_kind(user, connection_kind),
        connection_kind,
        Some(PlaybackLeaseRef::new(session_owner, playback_kind)),
    )?;

    let Some(provider_config) = provider_handle.allocation.get_provider_config() else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle));
        return None;
    };
    let Some((_provider_name, stream_url)) =
        select_provider_stream_url(request_url, input, &provider_config, false, &app_state.app_config).await
    else {
        app_state.connection_manager.release_provider_handle(Some(provider_handle));
        return None;
    };

    let session_token = if create_user_session {
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token: user_session_token,
                virtual_id,
                provider: &provider_config.name,
                stream_url: &stream_url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(connection_kind),
                socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
            })
            .await
    } else {
        user_session_token.to_string()
    };

    app_state.active_provider.refresh_adaptive_playback_lease(
        &provider_config.name,
        session_owner,
        playback_kind,
        reservation_ttl_secs,
    );

    Some(HlsEntryOriginAccountReservation {
        request_url: stream_url,
        session_token,
        provider_handle: Some(provider_handle),
        selected_provider_config: Some(provider_config),
    })
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn try_reserve_hls_virtual_entry_origin_account_for_redirect(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
) -> bool {
    let virtual_id = stream_identity.virtual_id();
    let session_token =
        create_playback_session_fingerprint(fingerprint, &user.username, virtual_id, PlaylistItemType::LiveHls, None);
    let (connection_admission, _, _) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        user,
        fingerprint,
        None,
        &session_token,
        false,
        EvictionReentryGuard::SocketPlayback { virtual_id: VirtualId::new(virtual_id) },
        false,
        false,
    )
    .await;
    if connection_admission.permission() == UserConnectionPermission::Exhausted {
        return false;
    }

    let Some(channel) = get_stream_channel(app_state, target, virtual_id).await else {
        return false;
    };
    let Ok(origin_playlist_url) =
        resolve_hls_origin_playlist_url(app_state, target, input, virtual_id, channel.url.as_ref()).await
    else {
        return false;
    };
    let Some(hls_cache_origin) = build_hls_origin_resolution(input, &origin_playlist_url) else {
        return false;
    };
    let Some(connection_kind) = connection_admission.kind() else {
        return false;
    };
    let (shared_hls_session_owner, reservation_ttl_secs) = if hls_cache_enabled_for_user(app_state, target, user) {
        let origin_source = build_hls_origin_source(input, stream_identity.stream_ref());
        let proxy_session_id = build_proxy_session_id(&origin_source.session_key(), &app_state.get_encrypt_secret());
        let reservation_ttl_secs = match app_state.hls.proxy.sessions().get_by_key(&origin_source.session_key()).await {
            Some(session) => hls_origin_account_reservation_ttl_secs_for_session(&session).await,
            None => hls_origin_account_reservation_ttl_secs_fallback(),
        };
        (Some(build_hls_origin_session_owner(&proxy_session_id)), reservation_ttl_secs)
    } else {
        (None, get_hls_session_ttl_secs(app_state))
    };
    let session_owner = shared_hls_session_owner.as_deref().unwrap_or(session_token.as_str());

    let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
        app_state,
        fingerprint,
        user,
        input,
        virtual_id,
        hls_cache_origin.session_entry_url.as_str(),
        &session_token,
        session_owner,
        PlaybackKind::LiveHls,
        reservation_ttl_secs,
        connection_admission.permission(),
        connection_kind,
        false,
    )
    .await
    else {
        return false;
    };

    app_state.connection_manager.release_provider_handle(reservation.provider_handle);
    true
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_provisioning_handoff_discontinuity(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
    access_lease_id: Option<&HlsAccessLeaseId>,
    now_ms: u64,
) -> bool {
    if !hls_cache_configured(app_state) {
        return false;
    }
    let origin_source = build_hls_origin_source(input, stream_identity.stream_ref());
    let Some(session) = app_state.hls.proxy.sessions().get_by_key(&origin_source.session_key()).await else {
        return false;
    };
    mark_hls_provisioning_handoff_discontinuity_once_for_session(
        app_state,
        &session,
        input,
        stream_identity.virtual_id(),
        access_lease_id,
        now_ms,
    )
    .await
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_provisioning_handoff_discontinuity_once_for_session(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: Option<&HlsAccessLeaseId>,
    now_ms: u64,
) -> bool {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    if !app_state.hls.provisioning.mark_handoff_once(
        &input.name,
        virtual_id,
        Some(&proxy_session_id),
        access_lease_id,
        now_ms,
    ) {
        debug!(
            "HLS provisioning handoff discontinuity already marked: proxy_session={}",
            safe_proxy_session_id(&proxy_session_id)
        );
        return false;
    }
    mark_hls_provisioning_handoff_discontinuity_for_session(session, now_ms).await;
    ensure_shared_hls_provisioning_handoff_gap(app_state, session, now_ms).await;
    true
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_provisioning_handoff_discontinuity_for_session(
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    let discontinuity_sequence = hls_provisioning_discontinuity_sequence(now_ms);
    let proxy_session_id = {
        let mut session = session.write().await;
        session.mark_pending_handoff_discontinuity(discontinuity_sequence);
        session.proxy_session_id.clone()
    };
    debug!(
        "HLS provisioning handoff discontinuity marked: proxy_session={} discontinuity_sequence={}",
        safe_proxy_session_id(&proxy_session_id),
        discontinuity_sequence
    );
}

pub(in crate::api::endpoints::hls_api) fn clear_hls_provisioning_handoff_consumer(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    virtual_id: u32,
    now_ms: u64,
) {
    if !app_state.hls.provisioning.take_ready_slot_for_consumer(&input.name, virtual_id, now_ms) {
        app_state.hls.provisioning.clear_consumer(&input.name, virtual_id);
    }
}

pub(in crate::api::endpoints::hls_api) async fn maybe_mark_hls_provisioning_handoff_for_canonical_manifest(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Option<u64> {
    if !app_state.hls.provisioning.has_consumer(&input.name, virtual_id, now_ms) {
        return None;
    }
    let previous_manifest_rendered_at_ms = latest_shared_hls_manifest_rendered_at_ms(session).await;
    mark_hls_provisioning_handoff_discontinuity_once_for_session(
        app_state,
        session,
        input,
        virtual_id,
        Some(access_lease_id),
        now_ms,
    )
    .await
    .then_some(previous_manifest_rendered_at_ms)
}

pub(in crate::api::endpoints::hls_api) async fn latest_shared_hls_manifest_rendered_at_ms(
    session: &HlsSessionHandle,
) -> u64 {
    let session = session.read().await;
    session
        .last_rendered_manifest
        .as_ref()
        .map_or(0, |rendered| rendered.rendered_at_ms)
        .max(session.transient.last_manifest_rendered_at_ms.unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api) async fn hls_panel_provisioning_poll_manifest_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
    original_hls_entry_path: &str,
    server_path: Option<&str>,
) -> axum::response::Response {
    hls_panel_provisioning_poll_response(
        app_state,
        fingerprint,
        user,
        target,
        input,
        stream_identity,
        original_hls_entry_path,
        server_path,
        HlsProvisioningPollResponseKind::Legacy,
    )
    .await
}

pub(in crate::api::endpoints::hls_api) enum HlsProvisioningPollResponseKind {
    Legacy,
}

impl HlsProvisioningPollResponseKind {
    pub(in crate::api::endpoints::hls_api) fn access_lease_id(&self) -> Option<&HlsAccessLeaseId> {
        match self {
            Self::Legacy => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn hls_panel_provisioning_poll_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    input: &ConfigInput,
    stream_identity: &HlsEntryStreamIdentity,
    ready_redirect_path: &str,
    server_path: Option<&str>,
    response_kind: HlsProvisioningPollResponseKind,
) -> axum::response::Response {
    let virtual_id = stream_identity.virtual_id();
    let now_ms = current_time_millis();
    app_state.hls.provisioning.touch_consumer(Arc::clone(&input.name), virtual_id, now_ms);

    let existing_status = app_state.hls.provisioning.consumer_status(&input.name, virtual_id, now_ms);

    if try_reserve_hls_virtual_entry_origin_account_for_redirect(
        app_state,
        fingerprint,
        user,
        target,
        input,
        stream_identity,
    )
    .await
    {
        mark_hls_provisioning_handoff_discontinuity(
            app_state,
            input,
            stream_identity,
            response_kind.access_lease_id(),
            now_ms,
        )
        .await;
        clear_hls_provisioning_handoff_consumer(app_state, input, virtual_id, current_time_millis());
        return hls_virtual_entry_redirect_response(ready_redirect_path, server_path);
    }

    let provisioning_enabled = can_provision_on_exhausted(app_state.as_ref(), input);
    if provisioning_enabled {
        start_hls_panel_provisioning_once(app_state, input);
    }

    let status = existing_status.unwrap_or(if provisioning_enabled {
        HlsProvisioningStatus::InProgress
    } else {
        HlsProvisioningStatus::ProviderExhausted
    });

    match status {
        HlsProvisioningStatus::Ready | HlsProvisioningStatus::InProgress => {
            hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                user,
                CustomVideoStreamType::Provisioning,
                StatusCode::SERVICE_UNAVAILABLE,
                Some(virtual_id),
            )
            .await
        }
        HlsProvisioningStatus::ProviderExhausted => {
            hls_custom_video_manifest_response_with_virtual_id(
                app_state,
                user,
                CustomVideoStreamType::ProviderConnectionsExhausted,
                StatusCode::SERVICE_UNAVAILABLE,
                Some(virtual_id),
            )
            .await
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_panel_provisioning_or_status_response(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    virtual_id: u32,
    _original_hls_entry_path: &str,
    server_path: Option<&str>,
    fallback_status: StatusCode,
) -> axum::response::Response {
    try_hls_panel_provisioning_manifest_response(
        app_state,
        user,
        input,
        virtual_id,
        HlsPanelProvisioningRedirectPaths { waiting_manifest_path: None },
        server_path,
        fallback_status,
    )
    .await
    .unwrap_or_else(|| fallback_status.into_response())
}

#[derive(Debug, Clone)]
pub(in crate::api::endpoints::hls_api) struct SharedHlsProvisioningSegmentPlan {
    pub(in crate::api::endpoints::hls_api) proxy_seq: u64,
    pub(in crate::api::endpoints::hls_api) physical_index: usize,
    pub(in crate::api::endpoints::hls_api) cache_key: SegmentCacheKey,
    pub(in crate::api::endpoints::hls_api) segment_kind: SharedHlsProvisioningLocalSegmentKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(in crate::api::endpoints::hls_api) enum SharedHlsProvisioningLocalSegmentKind {
    Provisioning,
    Gap,
}

pub(in crate::api::endpoints::hls_api) fn shared_hls_provisioning_segment_plans(
    session: &HlsSession,
    physical_segment_count: usize,
) -> Vec<SharedHlsProvisioningSegmentPlan> {
    let existing_provisioning_segments =
        session.segments.values().filter(|entry| is_hls_provisioning_segment(entry)).count();
    let append_count = if existing_provisioning_segments == 0 { 3 } else { 1 };
    let start_proxy_seq = session.proxy_next_seq.unwrap_or(0);
    (0..append_count)
        .filter_map(|offset| {
            let proxy_seq = start_proxy_seq.checked_add(u64::try_from(offset).ok()?)?;
            if session.segments.contains_key(&proxy_seq) {
                return None;
            }
            Some(SharedHlsProvisioningSegmentPlan {
                proxy_seq,
                physical_index: (existing_provisioning_segments + offset) % physical_segment_count,
                cache_key: SegmentCacheKey::new(session.proxy_session_id.clone(), proxy_seq, "ts"),
                segment_kind: SharedHlsProvisioningLocalSegmentKind::Provisioning,
            })
        })
        .collect()
}

pub(in crate::api::endpoints::hls_api) fn shared_hls_provisioning_segment_entry(
    plan: SharedHlsProvisioningSegmentPlan,
    content_length: u64,
    duration_ms: u64,
    now_ms: u64,
) -> SegmentEntry {
    let origin_epoch = match plan.segment_kind {
        SharedHlsProvisioningLocalSegmentKind::Provisioning => HLS_PROVISIONING_ORIGIN_EPOCH,
        SharedHlsProvisioningLocalSegmentKind::Gap => HLS_PROVISIONING_GAP_ORIGIN_EPOCH,
    };
    SegmentEntry {
        origin_key: OriginSegmentKey {
            origin_epoch,
            effective_host_id: 0,
            host_local_sequence: plan.proxy_seq,
            host_local_index: u32::try_from(plan.proxy_seq).unwrap_or(u32::MAX),
        },
        proxy_seq: plan.proxy_seq,
        duration_ms,
        proxy_file_ext: "ts".to_string(),
        content_type: "video/mp2t".to_string(),
        cache_key: plan.cache_key,
        discontinuity_before: false,
        program_date_time: None,
        daterange_tags_before: Vec::new(),
        origin_byte_range: None,
        map_ref: None,
        encryption: None,
        origin_fetch_ref: None,
        status: SegmentCacheStatus::Ready { content_length, ready_at_ms: now_ms },
        last_rendered_at_ms: None,
        access: Arc::new(CacheAccessState::new()),
    }
}

pub(in crate::api::endpoints::hls_api) async fn commit_shared_hls_provisioning_segments(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    plans: &[SharedHlsProvisioningSegmentPlan],
    provisioning_segments: &[TransportStreamBuffer],
) -> Option<Vec<(SharedHlsProvisioningSegmentPlan, u64, u64)>> {
    let mut committed = Vec::with_capacity(plans.len());
    for plan in plans {
        let video = provisioning_segments.get(plan.physical_index)?;
        let duration_ms = video.duration_ms().unwrap_or(HLS_PROVISIONING_SEGMENT_DURATION_MS);
        let metadata = match app_state
            .hls
            .proxy
            .segment_cache()
            .write_bytes_and_commit(&plan.cache_key, video.as_bytes())
            .await
        {
            Ok(metadata) => metadata,
            Err(err) => {
                let safe_proxy_session = {
                    let session_guard = session.read().await;
                    safe_proxy_session_id(&session_guard.proxy_session_id)
                };
                warn!(
                    "HLS provisioning segment cache commit failed for shared manifest: proxy_session={} seq={} error={err}",
                    safe_proxy_session, plan.proxy_seq
                );
                return None;
            }
        };
        committed.push((plan.clone(), metadata.size, duration_ms));
    }
    Some(committed)
}

pub(in crate::api::endpoints::hls_api) async fn ensure_shared_hls_provisioning_handoff_gap(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) -> bool {
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    let Some(provisioning_segments) = custom_stream_response
        .as_ref()
        .map(|response| response.panel_api_provisioning_hls_segments.clone())
        .filter(|segments| !segments.is_empty())
    else {
        return false;
    };
    let plan = {
        let session_guard = session.read().await;
        if !session_guard.segments.values().any(is_hls_provisioning_segment)
            || session_guard.segments.values().any(is_hls_provisioning_gap_segment)
        {
            return false;
        }
        let proxy_seq = session_guard.proxy_next_seq.unwrap_or(0);
        if session_guard.segments.contains_key(&proxy_seq) {
            return false;
        }
        let existing_provisioning_segments =
            session_guard.segments.values().filter(|entry| is_hls_provisioning_segment(entry)).count();
        SharedHlsProvisioningSegmentPlan {
            proxy_seq,
            physical_index: existing_provisioning_segments % provisioning_segments.len(),
            cache_key: SegmentCacheKey::new(session_guard.proxy_session_id.clone(), proxy_seq, "ts"),
            segment_kind: SharedHlsProvisioningLocalSegmentKind::Gap,
        }
    };
    let Some(committed) = commit_shared_hls_provisioning_segments(
        app_state,
        session,
        std::slice::from_ref(&plan),
        &provisioning_segments,
    )
    .await
    else {
        return false;
    };
    let mut session_guard = session.write().await;
    let mut inserted = false;
    for (plan, content_length, duration_ms) in committed {
        if session_guard.segments.contains_key(&plan.proxy_seq) {
            continue;
        }
        if session_guard.publishable_origin_head_proxy_seq.is_none() {
            session_guard.publishable_origin_head_proxy_seq = Some(plan.proxy_seq);
        }
        session_guard.publishable_origin_tail_proxy_seq = Some(plan.proxy_seq);
        session_guard.proxy_next_seq = Some(plan.proxy_seq.saturating_add(1));
        session_guard
            .segments
            .insert(plan.proxy_seq, shared_hls_provisioning_segment_entry(plan, content_length, duration_ms, now_ms));
        inserted = true;
    }
    if inserted {
        session_guard.target_duration = Some(HLS_PROVISIONING_TARGET_DURATION_SECS);
        session_guard.independent_segments = true;
    }
    inserted
}

pub(in crate::api::endpoints::hls_api) async fn hls_shared_provisioning_timeline_manifest_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
) -> Option<axum::response::Response> {
    let custom_stream_response = app_state.app_config.custom_stream_response.load();
    let provisioning_segments = custom_stream_response
        .as_ref()
        .map(|response| response.panel_api_provisioning_hls_segments.clone())
        .filter(|segments| !segments.is_empty())?;
    let now_ms = current_time_millis();
    let plans = {
        let session_guard = session.read().await;
        shared_hls_provisioning_segment_plans(&session_guard, provisioning_segments.len())
    };
    if plans.is_empty() {
        let mut session_guard = session.write().await;
        session_guard.render_and_store_manifest(now_ms).ok()?;
    } else {
        let committed =
            commit_shared_hls_provisioning_segments(app_state, session, &plans, &provisioning_segments).await?;
        let mut session_guard = session.write().await;
        for (plan, content_length, duration_ms) in committed {
            if session_guard.segments.contains_key(&plan.proxy_seq) {
                continue;
            }
            if session_guard.publishable_origin_head_proxy_seq.is_none() {
                session_guard.publishable_origin_head_proxy_seq = Some(plan.proxy_seq);
            }
            session_guard.publishable_origin_tail_proxy_seq = Some(plan.proxy_seq);
            session_guard.proxy_next_seq = Some(plan.proxy_seq.saturating_add(1));
            session_guard.segments.insert(
                plan.proxy_seq,
                shared_hls_provisioning_segment_entry(plan, content_length, duration_ms, now_ms),
            );
        }
        session_guard.target_duration = Some(HLS_PROVISIONING_TARGET_DURATION_SECS);
        session_guard.independent_segments = true;
        session_guard.render_and_store_manifest(now_ms).ok()?;
    }

    try_hls_cached_manifest_response(
        app_state,
        session,
        access_lease_id,
        access_lease_state,
        strip,
        server_path,
        HlsCachedManifestOptions::initial(Duration::ZERO),
        HlsRuntimeBandwidthLearningContext::Disabled,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn hls_shared_provisioning_or_provider_exhausted_response(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    username: &str,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
) -> axum::response::Response {
    let Some((_user, _target)) = app_state.app_config.get_target_for_username(username) else {
        return hls_canonical_retry_after_response();
    };
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let now_ms = current_time_millis();
    let provisioning_enabled = can_provision_on_exhausted(app_state.as_ref(), input);
    if provisioning_enabled {
        app_state.hls.provisioning.touch_consumer(Arc::clone(&input.name), virtual_id, now_ms);
        start_hls_panel_provisioning_once(app_state, input);
        if let Some(HlsProvisioningStatus::ProviderExhausted) =
            app_state.hls.provisioning.consumer_status(&input.name, virtual_id, now_ms)
        {
            return hls_runtime_or_standalone_custom_tail_response(
                app_state,
                session,
                &proxy_session_id,
                access_lease_id,
                HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        }
        if let Some(response) = hls_shared_provisioning_timeline_manifest_response(
            app_state,
            session,
            access_lease_id,
            access_lease_state,
            strip,
            server_path,
        )
        .await
        {
            return response;
        }
    }

    let provider_exhausted_custom_response_available = is_custom_video_stream_enabled(&app_state.app_config)
        && app_state
            .app_config
            .custom_stream_response
            .load()
            .as_ref()
            .and_then(|response| response.provider_connections_exhausted.as_ref())
            .is_some();
    if provider_exhausted_custom_response_available {
        return hls_runtime_or_standalone_custom_tail_response(
            app_state,
            session,
            &proxy_session_id,
            access_lease_id,
            HlsRuntimeCustomTailReason::ProviderConnectionsExhausted,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    }
    hls_canonical_retry_after_response()
}

pub(in crate::api::endpoints::hls_api) enum HlsProviderExhaustedResolution {
    RetryAcquire,
    Response(axum::response::Response),
}

#[allow(clippy::too_many_arguments)]
pub(in crate::api::endpoints::hls_api) async fn hls_provider_connections_exhausted_manifest_resolution(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    username: &str,
    input: &ConfigInput,
    virtual_id: u32,
    access_lease_id: &HlsAccessLeaseId,
    access_lease_state: HlsAccessLeaseState,
    strip: &crate::model::StripConfig,
    server_path: Option<&str>,
    allow_grace_hold: bool,
) -> HlsProviderExhaustedResolution {
    let proxy_session_id = session.read().await.proxy_session_id.clone();
    let grace_options = app_state.get_grace_options();
    if allow_grace_hold && grace_options.hold_stream && grace_options.period_millis > 0 {
        debug!(
            "HLS provider connections exhausted; holding canonical manifest for grace: proxy_session={} lease={} hold_ms={}",
            safe_proxy_session_id(&proxy_session_id),
            safe_hls_access_lease_id(access_lease_id),
            grace_options.period_millis
        );
        let capacity_notify = app_state.connection_manager.capacity_notified();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(grace_options.period_millis);
        let wake_reason = tokio::select! {
            () = capacity_notify.notified() => "capacity-notified",
            () = tokio::time::sleep_until(deadline) => "timeout",
        };
        debug!(
            "HLS provider connections exhausted grace hold completed: proxy_session={} lease={} reason={wake_reason}",
            safe_proxy_session_id(&proxy_session_id),
            safe_hls_access_lease_id(access_lease_id)
        );
        return HlsProviderExhaustedResolution::RetryAcquire;
    }

    HlsProviderExhaustedResolution::Response(
        hls_shared_provisioning_or_provider_exhausted_response(
            app_state,
            session,
            username,
            input,
            virtual_id,
            access_lease_id,
            access_lease_state,
            strip,
            server_path,
        )
        .await,
    )
}
