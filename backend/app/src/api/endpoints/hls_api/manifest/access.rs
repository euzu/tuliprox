use super::{
    build_hls_manifest_request_headers, build_hls_origin_resolution, build_hls_origin_source_for_playback,
    build_virtual_hls_entry_path, get_stream_channel, hls_canonical_status_response,
    hls_manifest_access_context_and_state, hls_manifest_terminal_preflight, hls_terminal_failed_closed_response,
    hls_terminal_playback_response, resolve_hls_origin_playlist_url, resolve_hls_terminal_manifest_state,
    try_hls_cache_canonical_manifest_response, HlsCacheManifestOrigin, HlsManifestRefreshOrdering,
    HlsManifestTerminalPreflight, HlsOriginEntryUrl, HlsProxyManifestPathParams,
};
use crate::{
    api::{
        api_utils::{is_hls_stream_share_enabled, member_playback_url},
        model::AppState,
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
};
use axum::{
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use shared::model::VirtualId;
use std::sync::Arc;
use tuliprox_hls::api::{
    HlsAccessContext, HlsAccessLease, HlsAccessLeaseId, HlsLeasePlaybackMode, HlsOriginSource,
    HlsRuntimeCustomTailReason, HlsSessionHandle, HlsSessionKey, HlsTerminalFailedClosedReason, ProxySessionId,
};
use tuliprox_repository::{failover_group_repository::GroupMember, target_stream_item::classify_virtual_id};

pub(in crate::api::endpoints::hls_api) async fn mark_successful_canonical_manifest_activity(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    if session.read().await.origin_playback_kind() == tuliprox_core::model::PlaybackKind::LiveHls {
        mark_hls_authorized_manifest_access(app_state, session, now_ms).await;
    } else {
        mark_hls_authorized_media_access(app_state, session, now_ms).await;
    }
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_authorized_manifest_access(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    session.write().await.mark_authorized_manifest_access(now_ms);
    app_state.hls.proxy.schedule_session_idle_for_handle(session).await;
}

pub(in crate::api::endpoints::hls_api) async fn mark_hls_authorized_media_access(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    now_ms: u64,
) {
    session.write().await.mark_authorized_media_access(now_ms);
    app_state.hls.proxy.schedule_session_idle_for_handle(session).await;
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_configured(app_state: &Arc<AppState>) -> bool {
    let config = app_state.app_config.config.load();
    config.reverse_proxy.as_ref().is_some_and(|reverse_proxy| reverse_proxy.hls_cache.is_some())
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_enabled_for_target(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
) -> bool {
    hls_cache_configured(app_state) && is_hls_stream_share_enabled(target)
}

pub(in crate::api::endpoints::hls_api) fn hls_cache_enabled_for_user(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    user: &ProxyUserCredentials,
) -> bool {
    // Recording headers can select different origin content from ordinary playback.
    !user.is_recording_proxy_user() && hls_cache_enabled_for_target(app_state, target)
}

/// Session key used by the canonical manifest/segment/resource path for `origin_source`.
///
/// A live failover-group target resolves to the logical group key; every other origin
/// (single-item live, archive/catch-up) keeps its per-origin key.
pub(in crate::api::endpoints::hls_api) async fn hls_cache_session_key_for_origin(
    app_state: &Arc<AppState>,
    context: &HlsAccessContext,
    origin_source: &HlsOriginSource,
) -> HlsSessionKey {
    // A live failover-group session's key is fixed at entry. Once the session exists, reuse its
    // key instead of re-deriving it from the group catalog: the catalog is published lazily and a
    // later poll that re-classifies the same virtual id as a group would compute a different key
    // and reject the entry's proxy session id.
    if let Some(session) = app_state.hls.proxy.sessions().get_by_proxy_session_id(&context.proxy_session_id).await {
        return session.read().await.key.clone();
    }
    if context.archive_origin_url.is_none() {
        if let Some((_, target)) = app_state.app_config.get_target_for_username(&context.username) {
            let classified = classify_virtual_id(
                &app_state.app_config,
                &app_state.playlists,
                &target,
                VirtualId::new(context.virtual_id),
            )
            .await;
            if let Some(group) = classified.group() {
                return HlsSessionKey::for_group(target.id, group.group_vid.get());
            }
        }
    }
    origin_source.session_key()
}

#[derive(Debug)]
pub(in crate::api::endpoints::hls_api) struct HlsAccessManifestRequestContext {
    pub(in crate::api::endpoints::hls_api) input: Arc<ConfigInput>,
    pub(in crate::api::endpoints::hls_api) hls_url: String,
    pub(in crate::api::endpoints::hls_api) session_entry_url: HlsOriginEntryUrl,
    pub(in crate::api::endpoints::hls_api) original_hls_entry_path: String,
    pub(in crate::api::endpoints::hls_api) origin_source: HlsOriginSource,
    pub(in crate::api::endpoints::hls_api) headers: HeaderMap,
    pub(in crate::api::endpoints::hls_api) server_path: Option<String>,
}

async fn shared_cache_session_origin_entry_url(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
) -> Option<String> {
    let session = app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await?;
    let session = session.read().await;
    let runtime = session.origin_account_runtime.as_ref()?;
    runtime.entry_url().or_else(|| runtime.raw_request_url()).filter(|url| !url.is_empty())
}

/// The account a shared-cache session is bound to, when it belongs to `member_input_name`.
async fn shared_cache_session_bound_account(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    member_input_name: &str,
) -> Option<Arc<str>> {
    let session = app_state.hls.proxy.sessions().get_by_proxy_session_id(proxy_session_id).await?;
    let binding = session.read().await.origin_account_binding.clone()?;
    (binding.input_name.as_ref() == member_input_name).then(|| Arc::clone(&binding.account_name))
}

/// Resolves the origin playlist URL for a Stalker group member on the shared-cache path. A session
/// that already holds its resolved entry URL reuses it so canonical manifest polls do not mint a new
/// Stalker portal link; otherwise the session's bound account (or the member's own account) resolves
/// one fresh link.
async fn resolve_stalker_group_member_origin_url(
    app_state: &Arc<AppState>,
    proxy_session_id: &ProxySessionId,
    member_input: &Arc<ConfigInput>,
    member: &GroupMember,
) -> Option<String> {
    if let Some(url) = shared_cache_session_origin_entry_url(app_state, proxy_session_id).await {
        return Some(url);
    }
    let account = shared_cache_session_bound_account(app_state, proxy_session_id, member_input.name.as_ref())
        .await
        .unwrap_or_else(|| Arc::clone(&member_input.name));
    member_playback_url(app_state, member_input, member, &account).await.map(|url| url.to_string())
}

async fn resolve_hls_playback_origin_playlist_url(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    input: &Arc<ConfigInput>,
    access_context: &HlsAccessContext,
    virtual_id: u32,
    group_member: Option<&GroupMember>,
    channel_url: &str,
) -> Result<String, StatusCode> {
    if let Some(archive_url) = access_context.archive_origin_url.as_ref() {
        return Ok(archive_url.clone());
    }
    let mut playlist_url = resolve_hls_origin_playlist_url(app_state, target, input, virtual_id, channel_url).await?;
    if playlist_url.is_empty() {
        if let Some(member) = group_member {
            let Some(member_input) = app_state.app_config.get_input_by_name(&member.input_name) else {
                return Err(StatusCode::NOT_FOUND);
            };
            if member_input.input_type.is_stalker() {
                playlist_url = resolve_stalker_group_member_origin_url(
                    app_state,
                    &access_context.proxy_session_id,
                    &member_input,
                    member,
                )
                .await
                .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
            }
        }
    }
    Ok(playlist_url)
}

#[allow(clippy::too_many_lines)]
pub(in crate::api::endpoints::hls_api) async fn resolve_hls_playback_manifest_request_context(
    app_state: &Arc<AppState>,
    access_context: &HlsAccessContext,
    req_headers: &HeaderMap,
) -> Result<HlsAccessManifestRequestContext, StatusCode> {
    let Some((user, target)) = app_state.app_config.get_target_for_username(&access_context.username) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if !hls_cache_enabled_for_target(app_state, &target) {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(input) = app_state.app_config.get_input_by_id(access_context.input_id) else {
        return Err(StatusCode::NOT_FOUND);
    };
    if app_state
        .active_users
        .is_user_blocked_for_stream(&user.username, VirtualId::new(access_context.virtual_id))
        .await
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut group_member: Option<GroupMember> = None;
    let mut playback_input = Arc::clone(&input);

    // A live failover-group shared-cache session keeps its bound member in `HlsSession.origin_source`;
    // the sealed `input_id`/`stream_ref` are only the member chosen at entry and may go stale after a
    // member handoff (R2). Once the group session exists, resolve the member from that bound state.
    let session = app_state.hls.proxy.sessions().get_by_proxy_session_id(&access_context.proxy_session_id).await;
    let (group_bound_origin, group_upstream_user_agent) = if let Some(session) = session.as_ref() {
        let session = session.read().await;
        if session.key.is_group_key() {
            let upstream_user_agent = session
                .origin_request_headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            (Some(session.origin_source.clone()), upstream_user_agent)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let playback_virtual_id = if access_context.archive_origin_url.is_none() {
        let classified = classify_virtual_id(
            &app_state.app_config,
            &app_state.playlists,
            &target,
            VirtualId::new(access_context.virtual_id),
        )
        .await;
        if let Some(group) = classified.group() {
            let (member_input_name, member_stream_ref) = group_bound_origin.as_ref().map_or_else(
                || (input.name.as_ref(), access_context.stream_ref.as_str()),
                |bound| (bound.input_name.as_ref(), bound.stream_ref.as_str()),
            );
            match group.find_member_vid(member_input_name, member_stream_ref) {
                Ok(member_vid) => {
                    let Some(member) = group.members.iter().find(|member| member.member_vid == member_vid).cloned()
                    else {
                        return Err(StatusCode::NOT_FOUND);
                    };
                    let Some(member_input) = app_state.app_config.get_input_by_name(&member.input_name) else {
                        return Err(StatusCode::NOT_FOUND);
                    };
                    group_member = Some(member);
                    playback_input = member_input;
                    member_vid.get()
                }
                Err(err) => {
                    log::warn!(
                        "HLS group member resolution failed: target={} virtual_id={} stream_ref={} input={} reason={}",
                        target.name,
                        access_context.virtual_id,
                        member_stream_ref,
                        member_input_name,
                        err.as_str()
                    );
                    return Err(StatusCode::NOT_FOUND);
                }
            }
        } else {
            access_context.virtual_id
        }
    } else {
        access_context.virtual_id
    };
    let Some(channel) = get_stream_channel(app_state, &target, playback_virtual_id).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    let origin_playlist_url = resolve_hls_playback_origin_playlist_url(
        app_state,
        &target,
        &playback_input,
        access_context,
        playback_virtual_id,
        group_member.as_ref(),
        channel.url.as_ref(),
    )
    .await?;
    let Some(hls_cache_origin) = build_hls_origin_resolution(&playback_input, &origin_playlist_url) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let origin_source = group_bound_origin.clone().unwrap_or_else(|| {
        build_hls_origin_source_for_playback(
            &playback_input,
            access_context.stream_ref.clone(),
            access_context.epg_reference_ts,
            Some(&origin_playlist_url),
        )
    });
    let Some(server_info) = app_state.app_config.get_user_server_info(&user) else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let upstream_user_agent = group_upstream_user_agent.as_deref().or(channel.upstream_user_agent.as_deref());
    let headers = build_hls_manifest_request_headers(
        &playback_input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        upstream_user_agent,
    );

    let original_hls_entry_path = build_virtual_hls_entry_path(&target, &input, &user, access_context.virtual_id);

    Ok(HlsAccessManifestRequestContext {
        input: playback_input,
        hls_url: hls_cache_origin.hls_url,
        session_entry_url: hls_cache_origin.session_entry_url,
        original_hls_entry_path,
        origin_source,
        headers,
        server_path: server_info.path.clone(),
    })
}

pub(in crate::api::endpoints::hls_api) async fn hls_manifest_preflight_refresh_ordering(
    app_state: &Arc<AppState>,
    session: &HlsSessionHandle,
    lease: &HlsAccessLease,
    proxy_session_id: &ProxySessionId,
    access_lease_id: &HlsAccessLeaseId,
    now_ms: u64,
) -> Result<HlsManifestRefreshOrdering, Box<axum::response::Response>> {
    match hls_manifest_terminal_preflight(session, lease, now_ms).await {
        HlsManifestTerminalPreflight::ServeCommittedPlayback => Err(Box::new(
            hls_terminal_playback_response(app_state, lease, proxy_session_id, access_lease_id).unwrap_or_else(|| {
                hls_terminal_failed_closed_response(HlsTerminalFailedClosedReason::RuntimeUnavailable)
            }),
        )),
        HlsManifestTerminalPreflight::BootstrapPendingLease => Ok(HlsManifestRefreshOrdering::Background),
        HlsManifestTerminalPreflight::RefreshBeforeTerminalEvaluation => {
            Ok(HlsManifestRefreshOrdering::AwaitBeforeTerminalEvaluation)
        }
        HlsManifestTerminalPreflight::EvaluateTerminal => resolve_hls_terminal_manifest_state(
            app_state,
            session,
            proxy_session_id,
            access_lease_id,
            lease.clone(),
            now_ms,
        )
        .await
        .map(|_| HlsManifestRefreshOrdering::Background),
        HlsManifestTerminalPreflight::FailClosed { reason } => {
            Err(Box::new(hls_terminal_failed_closed_response(reason)))
        }
    }
}

pub(in crate::api::endpoints::hls_api) async fn hls_proxy_manifest(
    fingerprint: Fingerprint,
    axum::extract::Path(params): axum::extract::Path<HlsProxyManifestPathParams>,
    axum::extract::State(app_state): axum::extract::State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let proxy_session_id = ProxySessionId(params.proxy_session_id);
    let access_lease_id = HlsAccessLeaseId(params.hls_access_lease_id);
    let now_ms = app_state.hls.proxy.now_ms();
    let access_lease_snapshot =
        app_state.hls.proxy.access_lease_response_snapshot(&access_lease_id, &proxy_session_id, now_ms).await;
    if let Some(lease) = access_lease_snapshot.as_ref() {
        let standalone_policy_response_required = lease.playback_mode == HlsLeasePlaybackMode::Ended
            && lease
                .runtime_policy_denial_reason()
                .is_some_and(HlsRuntimeCustomTailReason::permits_unpublished_lease_standalone_tail);
        if let (false, Some(response)) = (
            standalone_policy_response_required,
            hls_terminal_playback_response(&app_state, lease, &proxy_session_id, &access_lease_id),
        ) {
            return response;
        }
    }
    let session = app_state.hls.proxy.sessions().get_by_proxy_session_id(&proxy_session_id).await;
    if let Some(session) = session.as_ref() {
        app_state
            .hls
            .proxy
            .sync_session_access_lease_count_and_detach_if_needed(
                &app_state.active_users,
                &app_state.active_provider,
                session,
                &proxy_session_id,
                now_ms,
            )
            .await;
    }
    let (access_context, access_lease_state) = match hls_manifest_access_context_and_state(
        &app_state,
        &fingerprint,
        &proxy_session_id,
        &access_lease_id,
        access_lease_snapshot.as_ref(),
        now_ms,
    )
    .await
    {
        Ok(context_and_state) => context_and_state,
        Err(response) => return *response,
    };
    let refresh_ordering = if let (Some(session), Some(lease)) = (session.as_ref(), access_lease_snapshot.as_ref()) {
        match hls_manifest_preflight_refresh_ordering(
            &app_state,
            session,
            lease,
            &proxy_session_id,
            &access_lease_id,
            now_ms,
        )
        .await
        {
            Ok(ordering) => ordering,
            Err(response) => return *response,
        }
    } else {
        HlsManifestRefreshOrdering::Background
    };
    let request_context =
        match resolve_hls_playback_manifest_request_context(&app_state, &access_context, &headers).await {
            Ok(context) => context,
            Err(status) => return hls_canonical_status_response(status),
        };

    try_hls_cache_canonical_manifest_response(
        &app_state,
        &fingerprint,
        &access_context,
        &proxy_session_id,
        &access_lease_id,
        access_lease_state,
        HlsCacheManifestOrigin {
            raw_request_url: &request_context.hls_url,
            session_entry_url: request_context.session_entry_url.clone(),
            input: &request_context.input,
            origin_source: request_context.origin_source,
        },
        request_context.headers,
        request_context.server_path.as_deref(),
        &request_context.original_hls_entry_path,
        refresh_ordering,
    )
    .await
    .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::HeaderValue;
    use shared::{model::VirtualId, utils::Internable};
    use std::net::SocketAddr;
    use tuliprox_hls::api::{
        HlsAccessContext, HlsAccessLeaseId, HlsBoundAccountAcquireErrorKind, HlsOriginAccountRuntime,
        HlsOriginAccountWork, HlsOriginWorkClass, HlsPlaybackFamilyKey, HlsPreparedOriginAccount, HlsSessionHandle,
        ProxySessionId,
    };

    #[tokio::test]
    async fn hls_manifest_request_context_resolves_selected_group_member_url_and_user_agent() {
        let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().build().await;
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");
        let korath_member =
            env.group_record.members.iter().find(|m| m.input_name.as_ref() == "korath").expect("korath member");

        let target_path =
            crate::repository::get_target_storage_path(&env.app_state.app_config.config.load(), &env.target.name)
                .expect("target storage path");
        let mut group = (*env.group_record).clone();
        group.members[1].upstream_user_agent = Some(Arc::from("KorathAgent/1.0"));
        tuliprox_repository::FailoverGroupStore::write(&target_path, &[group], &[], None)
            .expect("write group with user agent");
        env.app_state.playlists.bump_failover_group_generation(&env.target.name);

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id: ProxySessionId("proxy-session".to_string()),
            input_id: korath_input.id,
            stream_ref: korath_member.input_stream_id.to_string(),
            virtual_id: env.group_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let request_context =
            resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new())
                .await
                .expect("manifest request context");

        let korath_addr = env.origins.get("korath").expect("korath origin").addr;
        assert!(
            request_context.hls_url.contains(&korath_addr.to_string()),
            "hls_url must point to selected member korath: {}",
            request_context.hls_url
        );
        assert!(
            request_context.hls_url.ends_with(&format!("/{}.m3u8", korath_member.provider_id)),
            "hls_url must end with member provider_id path (/{}.m3u8): {}",
            korath_member.provider_id,
            request_context.hls_url
        );
        assert_eq!(
            request_context.headers.get(axum::http::header::USER_AGENT),
            Some(&HeaderValue::from_static("KorathAgent/1.0")),
            "request_context headers must carry member upstream user agent"
        );
        // Original entry path preserves logical group VID
        assert!(
            request_context.original_hls_entry_path.contains(&env.group_vid.to_string()),
            "entry path must preserve logical group virtual_id: {}",
            request_context.original_hls_entry_path
        );
    }

    #[tokio::test]
    async fn hls_manifest_request_context_resolves_group_bound_member_from_session_state() {
        use crate::api::endpoints::hls_api::build_hls_origin_source;
        use tuliprox_hls::api::HlsSessionKey;

        let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().build().await;
        let solis_input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("solis input");
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");
        let korath_member =
            env.group_record.members.iter().find(|m| m.input_name.as_ref() == "korath").expect("korath member");

        let bound_origin = build_hls_origin_source(&korath_input, korath_member.input_stream_id.as_ref());
        let group_key = HlsSessionKey::for_group(env.target.id, env.group_vid);
        let (session, _) = env
            .app_state
            .hls
            .proxy
            .get_or_create_session_with_source_and_outcome(
                group_key,
                bound_origin,
                &env.app_state.get_encrypt_secret(),
                0,
            )
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        {
            let mut session = session.write().await;
            session
                .origin_request_headers
                .insert(axum::http::header::USER_AGENT, HeaderValue::from_static("BoundKorathAgent/2.0"));
        }

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        // Seal the logical group identity (solis representative + group VID), not the bound member,
        // to prove canonical context resolution reads the member from the group session state.
        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id,
            input_id: solis_input.id,
            stream_ref: "101".to_string(),
            virtual_id: env.group_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let request_context =
            resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new())
                .await
                .expect("manifest request context");

        let korath_addr = env.origins.get("korath").expect("korath origin").addr;
        assert!(
            request_context.hls_url.contains(&korath_addr.to_string()),
            "bound member URL must come from session state: {}",
            request_context.hls_url
        );
        assert_eq!(
            request_context.headers.get(axum::http::header::USER_AGENT),
            Some(&HeaderValue::from_static("BoundKorathAgent/2.0")),
            "user agent must come from session state"
        );
        assert!(
            request_context.original_hls_entry_path.contains(&env.group_vid.to_string()),
            "entry path must preserve logical group virtual_id"
        );
    }

    #[tokio::test]
    async fn hls_manifest_request_context_missing_group_member_fails_closed_404() {
        let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().build().await;
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id: ProxySessionId("proxy-session".to_string()),
            input_id: korath_input.id,
            stream_ref: "nonexistent-stream-ref".to_string(),
            virtual_id: env.group_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let Err(err) =
            resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new()).await
        else {
            panic!("missing member must fail closed with 404");
        };

        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hls_manifest_request_context_ambiguous_group_members_fails_closed_404() {
        let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().build().await;
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");
        let korath_member =
            env.group_record.members.iter().find(|m| m.input_name.as_ref() == "korath").expect("korath member");

        let target_path =
            crate::repository::get_target_storage_path(&env.app_state.app_config.config.load(), &env.target.name)
                .expect("target storage path");
        let mut group = (*env.group_record).clone();
        let mut duplicate_member = korath_member.clone();
        duplicate_member.member_vid = VirtualId::new(999);
        group.members.push(duplicate_member);
        tuliprox_repository::FailoverGroupStore::write(&target_path, &[group], &[], None)
            .expect("write ambiguous group");
        env.app_state.playlists.bump_failover_group_generation(&env.target.name);

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id: ProxySessionId("proxy-session".to_string()),
            input_id: korath_input.id,
            stream_ref: korath_member.input_stream_id.to_string(),
            virtual_id: env.group_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let Err(err) =
            resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new()).await
        else {
            panic!("ambiguous member must fail closed with 404");
        };

        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hls_manifest_request_context_non_group_item_unchanged() {
        let env = GroupPlaybackEnv::new(&["solis", "korath"]).hls_cache().build().await;
        let solis_input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("solis input");

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id: ProxySessionId("proxy-session".to_string()),
            input_id: solis_input.id,
            stream_ref: "101".to_string(),
            virtual_id: env.plain_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let request_context =
            resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new())
                .await
                .expect("manifest request context for non-group item");

        let solis_addr = env.origins.get("solis").expect("solis origin").addr;
        assert!(
            request_context.hls_url.contains(&solis_addr.to_string()),
            "hls_url must point to non-group origin solis: {}",
            request_context.hls_url
        );
    }

    #[tokio::test]
    async fn hls_manifest_request_context_resolves_stalker_group_member_non_empty_origin_url() {
        use crate::api::endpoints::hls_api::tests::failover_group::publish_stalker_live_item;

        let env = GroupPlaybackEnv::builder(&["solis"])
            .hls_cache()
            .stalker("solis")
            .stalker_retry_origin("solis", "/live/solis.m3u8")
            .build()
            .await;
        let solis_input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("solis input");
        let solis_member =
            env.group_record.members.iter().find(|m| m.input_name.as_ref() == "solis").expect("solis member");
        assert!(solis_member.url.is_empty(), "stalker group member URL is empty before resolution");
        publish_stalker_live_item(&env, &solis_input).await;

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id: ProxySessionId("proxy-session-stalker".to_string()),
            input_id: solis_input.id,
            stream_ref: solis_member.input_stream_id.to_string(),
            virtual_id: env.group_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let request_context =
            resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new())
                .await
                .expect("manifest request context must resolve for stalker member");

        assert!(!request_context.hls_url.is_empty(), "hls_url must not be empty for stalker member");
        let solis_addr = env.origins.get("solis").expect("solis origin").addr;
        assert!(
            request_context.hls_url.contains(&solis_addr.to_string()),
            "hls_url must point to resolved stalker origin: {}",
            request_context.hls_url
        );
    }

    struct ReuseOnlyRuntime {
        entry_url: String,
    }

    impl std::fmt::Debug for ReuseOnlyRuntime {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ReuseOnlyRuntime").field("entry_url", &self.entry_url).finish()
        }
    }

    impl HlsOriginAccountRuntime for ReuseOnlyRuntime {
        fn entry_url(&self) -> Option<String> { Some(self.entry_url.clone()) }

        fn prepare<'a>(
            &'a self,
            _session: &'a HlsSessionHandle,
            _work: HlsOriginAccountWork,
            _class: HlsOriginWorkClass,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<HlsPreparedOriginAccount, HlsBoundAccountAcquireErrorKind>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { unreachable!("reuse-only runtime is never prepared") })
        }
    }

    #[tokio::test]
    async fn hls_manifest_request_context_reuses_existing_session_origin_url_for_stalker_member() {
        use crate::api::endpoints::hls_api::{
            build_hls_origin_source, tests::failover_group::publish_stalker_live_item,
        };

        let env = GroupPlaybackEnv::builder(&["solis"])
            .hls_cache()
            .stalker("solis")
            .stalker_retry_origin("solis", "/live/solis.m3u8")
            .build()
            .await;
        let solis_input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("solis input");
        let solis_member =
            env.group_record.members.iter().find(|m| m.input_name.as_ref() == "solis").expect("solis member");
        publish_stalker_live_item(&env, &solis_input).await;

        let origin_source = build_hls_origin_source(&solis_input, solis_member.input_stream_id.as_ref());
        let (session, _) = env
            .app_state
            .hls
            .proxy
            .get_or_create_session_with_source_and_outcome(
                origin_source.session_key(),
                origin_source,
                &env.app_state.get_encrypt_secret(),
                0,
            )
            .await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();
        let reused_entry_url = "http://stub.example/live/reused.m3u8".to_string();
        {
            let mut session_guard = session.write().await;
            session_guard.origin_account_runtime =
                Some(std::sync::Arc::new(ReuseOnlyRuntime { entry_url: reused_entry_url.clone() }));
        }

        let addr = SocketAddr::from(([127, 0, 0, 1], 12345));
        let fingerprint = Fingerprint::new("fp-viewer".to_string(), "127.0.0.1".to_string(), addr);

        let access_context = HlsAccessContext {
            username: "viewer".to_string(),
            user_session_token: "hls-session-token".to_string(),
            proxy_session_id,
            input_id: solis_input.id,
            stream_ref: solis_member.input_stream_id.to_string(),
            virtual_id: env.group_vid,
            known_bitrate_bps: None,
            lease_id: HlsAccessLeaseId("access-lease".to_string()),
            family_key: HlsPlaybackFamilyKey::new("viewer", fingerprint.key),
            epg_reference_ts: None,
            archive_origin_url: None,
        };

        let first = resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new())
            .await
            .expect("first manifest context");
        let second = resolve_hls_playback_manifest_request_context(&env.app_state, &access_context, &HeaderMap::new())
            .await
            .expect("second manifest context");

        assert_eq!(first.hls_url, reused_entry_url);
        assert_eq!(second.hls_url, reused_entry_url);
        assert_eq!(env.origin_hits("solis"), 0, "reusing the session origin URL must not contact the Stalker portal");
    }
}
