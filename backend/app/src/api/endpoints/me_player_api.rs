use crate::{
    api::{
        api_utils::{failover_group::load_group_playlist_item, failover_group_runtime::SwitchError, kick_stream},
        auth_middleware::ApiUserContext,
        endpoints::{
            hls_api::{
                find_hls_group_playback, manual_switch_legacy_hls_group_session, LegacySwitchOutcome,
                ResolvedHlsGroupPlayback,
            },
            stream_failover_api::{build_user_failover_members_dto, resolve_failover_stream, ResolvedFailoverStream},
        },
        model::AppState,
    },
    auth::Fingerprint,
};
use axum::{
    extract::{Path, Query, State},
    http::{header::RETRY_AFTER, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use shared::{
    model::{
        JoinableStreamDto, PlayerRecordingLevel, PlaylistItemType, StreamChannel, StreamFailoverSwitchRequest,
        StreamInfo, UUIDType, UserStreamDto, VirtualId, PERMISSION_NAMES,
    },
    utils::{concat_path_leading_slash, extract_extension_from_url, generate_provider_playlist_uuid},
};
use std::sync::Arc;
use tuliprox_core::model::PlaybackKind;
use tuliprox_hls::api::HlsManualSwitchError;
use tuliprox_repository::{
    failover_group_repository::FailoverGroupRecord,
    target_stream_item::{classify_virtual_id, ResolvedStreamTarget},
    TargetUuidIndex,
};

fn playback_kind_protocol(kind: PlaybackKind) -> &'static str {
    match kind {
        PlaybackKind::LiveDash => "dash",
        PlaybackKind::LiveHls => "hls",
        _ => "ts",
    }
}

/// Discovers the effective capabilities and limits of the authenticated playlist user.
pub async fn get_me_capabilities(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
) -> Json<shared::model::PlayerCapabilitiesDto> {
    let features = &api_user.user.player_features;
    let active_connections = app_state.active_users.user_connections(&api_user.user.username).await;
    let free_shared_joins_in_use =
        app_state.shared_stream_manager.user_free_shared_joins(&api_user.user.username).await;
    let shared_joins = app_state.active_users.user_shared_joins(&api_user.user.username).await;

    let effective_permission_names = PERMISSION_NAMES
        .iter()
        .filter(|(_, perm)| api_user.claims.permissions.contains(*perm))
        .map(|(name, _)| (*name).to_string())
        .collect();

    let share_live_streams = crate::api::api_utils::is_stream_share_enabled(PlaylistItemType::Live, &api_user.target);

    let recording_enabled =
        crate::api::model::recording::recording_supervisor::recording_enabled(&app_state.app_config);

    let max_recording_rules =
        matches!(features.recording, PlayerRecordingLevel::Private | PlayerRecordingLevel::Shared)
            .then_some(features.max_recording_rules);

    let dto = shared::model::PlayerCapabilitiesDto {
        source_switch: features.source_switch,
        joinable_streams: features.joinable_streams,
        recording: features.recording,
        recording_quota_bytes: features.recording_quota_bytes,
        max_concurrent_recordings: features.max_concurrent_recordings,
        max_recording_rules,
        free_shared_joins: features.free_shared_joins,
        max_free_shared_joins: features.max_free_shared_joins,
        watch_progress: features.watch_progress,
        event_stream: features.event_stream,
        stream_split: features.source_switch,
        effective_permissions: effective_permission_names,
        max_connections: api_user.user.max_connections,
        active_connections,
        shared_joins,
        free_shared_joins_in_use,
        share_live_streams,
        source_switch_protocols: vec!["ts".to_string(), "hls".to_string()],
        recording_enabled,
    };
    Json(dto)
}

/// Resolves the current member virtual ID for a playback stream, checking `member_info`
/// first and falling back to HLS origin/session state for HLS shared cache streams.
async fn resolve_stream_member_vid(
    app_state: &AppState,
    stream: &StreamInfo,
    group: Option<&FailoverGroupRecord>,
) -> Option<u32> {
    if let Some(vid) = stream.member_info.as_ref().and_then(|m| m.member_vid) {
        return Some(vid);
    }
    let kind = PlaybackKind::classify(stream.channel.item_type, extract_extension_from_url(&stream.channel.url));
    if kind != PlaybackKind::LiveHls {
        return None;
    }
    let group_vid = stream.channel.group_virtual_id.unwrap_or(stream.channel.virtual_id);
    let playback = find_hls_group_playback(app_state, stream, stream.channel.target_id, group_vid).await?;

    let resolved_group;
    let group_ref = if let Some(g) = group {
        g
    } else {
        let target = app_state.app_config.get_target_by_id(stream.channel.target_id)?;
        let classified =
            classify_virtual_id(&app_state.app_config, &app_state.playlists, &target, VirtualId::new(group_vid)).await;
        let ResolvedStreamTarget::Group(g) = classified else {
            return None;
        };
        resolved_group = g;
        &resolved_group
    };

    match playback {
        ResolvedHlsGroupPlayback::Cache(handle) => {
            let guard = handle.read().await;
            group_ref
                .members
                .iter()
                .find(|m| {
                    m.input_name.as_ref() == guard.origin_source.input_name.as_ref()
                        && m.input_stream_id.as_ref() == guard.origin_source.stream_ref.as_str()
                })
                .map(|m| m.member_vid.get())
        }
        ResolvedHlsGroupPlayback::Legacy(user_session) => {
            let binding = user_session.member_binding.as_ref()?;
            group_ref.members.get(binding.member_index).map(|m| m.member_vid.get())
        }
    }
}

/// Lists all active streams belonging to the authenticated playlist user.
pub async fn get_me_streams(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
) -> Json<Vec<UserStreamDto>> {
    let streams = app_state.active_users.user_streams(&api_user.user.username).await;
    let mut dtos = Vec::with_capacity(streams.len());
    for s in streams {
        let member_vid = resolve_stream_member_vid(&app_state, &s, None).await;
        let kind = PlaybackKind::classify(s.channel.item_type, extract_extension_from_url(&s.channel.url));
        let protocol = playback_kind_protocol(kind).to_string();
        dtos.push(UserStreamDto {
            uid: s.uid,
            virtual_id: s.channel.virtual_id,
            group_virtual_id: s.channel.group_virtual_id,
            member_vid,
            title: s.channel.title.to_string(),
            item_type: s.channel.item_type,
            protocol,
            shared: s.channel.shared,
            session_active: !s.preserved,
            started_at: s.started_at,
            user_agent: s.user_agent,
            session_token: s.session_token,
        });
    }
    Json(dtos)
}

/// Terminates an own stream and releases its connection slot.
pub async fn delete_me_stream(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path(uid): Path<u32>,
) -> Response {
    let Some(stream) = app_state.active_users.find_panel_stream_by_uid(uid).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if stream.username != api_user.user.username {
        return StatusCode::NOT_FOUND.into_response();
    }

    kick_stream(&app_state, &stream, 0, shared::model::StreamEndedReason::Stopped).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Returns failover group members for an active playback stream of the caller.
pub async fn get_me_stream_failover_members(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path(uid): Path<u32>,
) -> Response {
    if !api_user.user.player_features.source_switch {
        return (StatusCode::FORBIDDEN, "feature_disabled").into_response();
    }

    let ResolvedFailoverStream { stream, group, target, .. } =
        match resolve_failover_stream(&app_state, uid, Some(&api_user.user.username), true).await {
            Ok(resolved) => resolved,
            Err((code, msg)) => return (code, msg).into_response(),
        };

    let current_member_vid = resolve_stream_member_vid(&app_state, &stream, Some(&group)).await;
    let group_name = if stream.channel.title.is_empty() { group.key.as_ref() } else { stream.channel.title.as_ref() };
    let now = std::time::Instant::now();
    let dto = build_user_failover_members_dto(
        &group,
        current_member_vid,
        group_name,
        &target.name,
        &app_state.member_health,
        now,
    );
    Json(dto).into_response()
}

/// Returns redacted failover group members for a channel virtual id before playback starts.
pub async fn get_me_channel_failover_members(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path(vid): Path<u32>,
) -> Response {
    if !api_user.user.player_features.source_switch {
        return (StatusCode::FORBIDDEN, "feature_disabled").into_response();
    }

    let classified =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, &api_user.target, VirtualId::new(vid)).await;

    let ResolvedStreamTarget::Group(group) = classified else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let Some(item) =
        load_group_playlist_item(&app_state.app_config, &app_state.playlists, &api_user.target, group.group_vid.get())
            .await
    else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if !api_user.user.allows_cluster(shared::model::XtreamCluster::Live)
        || !api_user.user.allows_item_type(item.header.item_type)
        || !crate::api::endpoints::hls_api::user_allows_playlist_filter(&api_user.user, &item)
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    let group_name = if item.header.title.is_empty() { group.key.to_string() } else { item.header.title.to_string() };

    let now = std::time::Instant::now();
    let dto = build_user_failover_members_dto(
        &group,
        None,
        &group_name,
        &api_user.target.name,
        &app_state.member_health,
        now,
    );
    Json(dto).into_response()
}

fn cooldown_429_response(retry_after: std::time::Duration) -> Response {
    let secs = retry_after.as_secs().saturating_add(u64::from(retry_after.subsec_nanos() > 0)).max(1);
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "switch_cooldown").into_response();
    response.headers_mut().insert(RETRY_AFTER, HeaderValue::from(secs));
    response
}

async fn switch_hls_failover_stream(
    app_state: &Arc<AppState>,
    api_user: &ApiUserContext,
    stream: &StreamInfo,
    group: Arc<FailoverGroupRecord>,
    group_vid: u32,
    target_member_index: usize,
) -> Response {
    let playback = find_hls_group_playback(app_state, stream, stream.channel.target_id, group_vid).await;

    let Some(playback) = playback else {
        return (StatusCode::BAD_GATEWAY, "playback session not active").into_response();
    };

    match playback {
        ResolvedHlsGroupPlayback::Cache(session) => {
            let (proxy_session_id, runtime) = {
                let guard = session.read().await;
                (guard.proxy_session_id.clone(), guard.origin_account_runtime.clone())
            };
            let now_ms = tuliprox_core::utils::current_time_millis();
            let active_tokens =
                app_state.hls.proxy.non_terminal_user_session_tokens_for_proxy_session(&proxy_session_id, now_ms).await;
            let Some(caller_token) = stream.session_token.as_deref() else {
                return (StatusCode::CONFLICT, "shared_stream_switch_refused").into_response();
            };
            let mut live_tokens = std::collections::HashSet::new();
            for tok in active_tokens {
                if !app_state.active_users.is_session_ended(&tok).await {
                    live_tokens.insert(tok);
                }
            }
            if live_tokens.len() != 1 || !live_tokens.contains(caller_token) {
                return (StatusCode::CONFLICT, "shared_stream_switch_refused").into_response();
            }

            let Some(runtime) = runtime else {
                return (StatusCode::BAD_GATEWAY, "origin runtime not available").into_response();
            };

            match runtime.request_member_handoff(&session, target_member_index).await {
                Ok(()) => StatusCode::OK.into_response(),
                Err(err) => match err {
                    HlsManualSwitchError::InProgress => (StatusCode::CONFLICT, "switch_in_progress").into_response(),
                    HlsManualSwitchError::MemberNotFound => {
                        (StatusCode::BAD_REQUEST, "member not in group").into_response()
                    }
                    HlsManualSwitchError::Cooldown { retry_after } => cooldown_429_response(retry_after),
                    _ => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
                },
            }
        }
        ResolvedHlsGroupPlayback::Legacy(user_session) => {
            let Some(session_token) = stream.session_token.as_deref() else {
                return (StatusCode::BAD_GATEWAY, "session token missing").into_response();
            };
            let fingerprint = Fingerprint::new(
                format!("{}-{}", stream.username, stream.addr),
                stream.addr.ip().to_string(),
                stream.addr,
            );
            let mut playback_headers = HeaderMap::new();
            if !stream.user_agent.is_empty() {
                if let Ok(ua_val) = HeaderValue::from_str(&stream.user_agent) {
                    playback_headers.insert(axum::http::header::USER_AGENT, ua_val);
                }
            }
            let outcome = manual_switch_legacy_hls_group_session(
                app_state,
                &fingerprint,
                &api_user.user,
                &api_user.target,
                &playback_headers,
                group,
                &user_session,
                session_token,
                target_member_index,
            )
            .await;

            match outcome {
                LegacySwitchOutcome::Committed => StatusCode::OK.into_response(),
                LegacySwitchOutcome::SwitchInFlight => (StatusCode::CONFLICT, "switch_in_progress").into_response(),
                LegacySwitchOutcome::Cooldown { retry_after } => cooldown_429_response(retry_after),
                LegacySwitchOutcome::MemberNotFound => (StatusCode::BAD_REQUEST, "member not in group").into_response(),
                _ => (StatusCode::BAD_GATEWAY, "legacy switch failed").into_response(),
            }
        }
    }
}

async fn switch_ts_failover_stream(
    app_state: &AppState,
    stream: &StreamInfo,
    group_vid: u32,
    target_member_vid: u32,
) -> Response {
    let is_shared = stream.channel.shared;
    let target_id = stream.channel.target_id;
    if is_shared {
        let key = crate::api::api_utils::failover_group::group_shared_key(target_id, group_vid);
        let subscriber_count = app_state.shared_stream_manager.subscriber_count(&key).await;
        if subscriber_count != 1 {
            return (StatusCode::CONFLICT, "shared_stream_switch_refused").into_response();
        }
    }

    match app_state
        .failover_switch_registry
        .switch_stream(stream.uid, is_shared, target_id, group_vid, target_member_vid)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => match err {
            SwitchError::InProgress => (StatusCode::CONFLICT, "switch_in_progress").into_response(),
            SwitchError::MemberNotFound => (StatusCode::BAD_REQUEST, "member not in group").into_response(),
            SwitchError::Cooldown { retry_after } => cooldown_429_response(retry_after),
            _ => (StatusCode::BAD_GATEWAY, err.to_string()).into_response(),
        },
    }
}

async fn resolve_stream_subscriber_count(app_state: &AppState, stream: &StreamInfo) -> usize {
    let client_vid = stream.channel.client_virtual_id();
    let group_key = crate::api::api_utils::failover_group::group_shared_key(stream.channel.target_id, client_vid);
    let group_count = app_state.shared_stream_manager.subscriber_count(&group_key).await;
    if group_count > 0 {
        return group_count;
    }
    app_state.shared_stream_manager.subscriber_count(&stream.channel.url).await
}

/// Switches a running own failover group stream to a target member.
pub async fn post_me_stream_failover_switch(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path(uid): Path<u32>,
    Json(payload): Json<StreamFailoverSwitchRequest>,
) -> Response {
    if !api_user.user.player_features.source_switch {
        return (StatusCode::FORBIDDEN, "feature_disabled").into_response();
    }

    let ResolvedFailoverStream { stream, group, group_vid, .. } =
        match resolve_failover_stream(&app_state, uid, Some(&api_user.user.username), true).await {
            Ok(resolved) => resolved,
            Err((code, msg)) => return (code, msg).into_response(),
        };

    let Some(target_member_index) = group.members.iter().position(|m| m.member_vid.get() == payload.member_vid) else {
        return (StatusCode::BAD_REQUEST, "member not in group").into_response();
    };

    // Switch-after-switch is limited by the switch drivers (TS group switcher, HLS cache handoff, HLS legacy
    // session), so the admin and the user route share one rule; a driver cooldown answers 429 below. The
    // handler only honours the cooldown a split records, because no driver stands behind a split.
    if let Err(retry_after) = app_state.active_users.check_stream_switch_cooldown(&api_user.user.username, stream.uid) {
        return cooldown_429_response(retry_after);
    }
    let kind = PlaybackKind::classify(stream.channel.item_type, extract_extension_from_url(&stream.channel.url));
    if kind == PlaybackKind::LiveHls {
        switch_hls_failover_stream(&app_state, &api_user, &stream, group, group_vid, target_member_index).await
    } else {
        switch_ts_failover_stream(&app_state, &stream, group_vid, payload.member_vid).await
    }
}

/// Splits an active shared stream session off into a dedicated stream connection.
///
/// Returns the reconnect URL with `dedicated=1` for the client to reopen the channel.
/// No provider connections are pre-reserved: the client must close its existing
/// shared stream connection and open the new dedicated URL, allowing existing admission
/// controls to decide capacity and slot allocation.
pub async fn post_me_stream_split(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Path(uid): Path<u32>,
) -> Response {
    if !api_user.user.player_features.source_switch {
        return (StatusCode::FORBIDDEN, "feature_disabled").into_response();
    }

    let Some(stream) = app_state.active_users.find_panel_stream_by_uid(uid).await else {
        return (StatusCode::NOT_FOUND, "stream not found").into_response();
    };

    if stream.username != api_user.user.username {
        return (StatusCode::NOT_FOUND, "stream not found").into_response();
    }

    let kind = PlaybackKind::classify(stream.channel.item_type, extract_extension_from_url(&stream.channel.url));
    if kind != PlaybackKind::LiveTs {
        return (StatusCode::CONFLICT, "split_unsupported_protocol").into_response();
    }

    if !stream.channel.shared {
        return (StatusCode::CONFLICT, "stream_not_shared").into_response();
    }

    let subscriber_count = resolve_stream_subscriber_count(&app_state, &stream).await;
    if subscriber_count <= 1 {
        return (StatusCode::CONFLICT, "stream_not_shared").into_response();
    }

    if let Err(retry_after) = app_state.active_users.check_stream_switch_cooldown(&api_user.user.username, stream.uid) {
        return cooldown_429_response(retry_after);
    }

    app_state.active_users.record_stream_switch_cooldown(&api_user.user.username, stream.uid);

    let client_vid = stream.channel.client_virtual_id();
    let url = if api_user.target.has_output(shared::model::TargetType::Xtream) {
        format!("/live/{}/{}/{client_vid}.ts?dedicated=1", api_user.user.username, api_user.user.password)
    } else {
        format!(
            "/{}/live/{}/{}/{client_vid}.ts?dedicated=1",
            tuliprox_repository::storage_const::M3U_STREAM_PATH,
            api_user.user.username,
            api_user.user.password
        )
    };

    let dto = shared::model::SplitStreamResponseDto { reconnect_required: true, url };

    Json(dto).into_response()
}

/// Determines whether the caller requires a free connection slot to join a shared stream.
///
/// Returns true when the caller has no free connection slot right now (`max_connections > 0`
/// and active counted connections >= max) and free shared joins are not active.
fn caller_needs_free_slot(
    user: &tuliprox_core::model::ProxyUserCredentials,
    active_connections: u32,
    free_joins_in_use: u32,
) -> bool {
    let at_limit = user.max_connections > 0 && active_connections >= user.max_connections;
    if !at_limit {
        return false;
    }
    !crate::api::api_utils::free_join_available(user, free_joins_in_use)
}

/// Provider identity of a running channel as it is hashed into a target's UUID index.
///
/// Provider item UUIDs hash the input name, the provider stream id and the item type, so the
/// same input item carries the same UUID in every target that includes it. Playback-only live
/// variants share the identity of the persisted `Live` item. M3U items whose provider id is not
/// numeric do not round-trip through `provider_id`; they stay unlisted across targets instead of
/// resolving to a guessed identity.
fn channel_provider_uuid(channel: &StreamChannel) -> UUIDType {
    let item_type = match channel.item_type {
        PlaylistItemType::LiveHls | PlaylistItemType::LiveDash => PlaylistItemType::Live,
        other => other,
    };
    generate_provider_playlist_uuid(&channel.input_name, &channel.provider_id.to_string(), item_type)
}

/// Whether a playback of `kind` from `target` consults running shared streams.
fn target_joins_shared_streams(
    target: &tuliprox_core::model::ConfigTarget,
    item_type: PlaylistItemType,
    kind: PlaybackKind,
) -> bool {
    match kind {
        PlaybackKind::LiveHls => crate::api::api_utils::is_hls_stream_share_enabled(target),
        PlaybackKind::LiveTs => crate::api::api_utils::is_stream_share_enabled(item_type, target),
        _ => false,
    }
}

/// Builds one joinable entry from the caller-target item behind `caller_vid`.
///
/// The checks mirror what a plain playback of `caller_vid` accepts. A stream of another target
/// additionally needs the identical provider identity (same input and upstream URL) and a
/// caller target that joins shared streams, because playback would not join it otherwise.
/// The returned entry only carries caller-target data.
async fn build_joinable_entry(
    app_state: &Arc<AppState>,
    api_user: &ApiUserContext,
    channel: &StreamChannel,
    caller_vid: u32,
    kind: PlaybackKind,
    cross_target: bool,
    requires_free_slot: bool,
) -> Option<JoinableStreamDto> {
    let item =
        load_group_playlist_item(&app_state.app_config, &app_state.playlists, &api_user.target, caller_vid).await?;

    if cross_target {
        // Sharing is keyed by the upstream URL, so only an identical input url joins.
        if item.header.input_name != channel.input_name || item.header.url != channel.url {
            return None;
        }
        if !target_joins_shared_streams(&api_user.target, item.header.item_type, kind) {
            return None;
        }
    }

    if !api_user.user.allows_item_type(item.header.item_type)
        || !crate::api::endpoints::hls_api::user_allows_playlist_filter(&api_user.user, &item)
    {
        return None;
    }

    // A foreign stream's title is not the caller's data, so it never stands in for a caller title.
    let fallback_title = if cross_target { "" } else { channel.title.as_ref() };
    let title = if !item.header.title.is_empty() {
        item.header.title.to_string()
    } else if !item.header.name.is_empty() {
        item.header.name.to_string()
    } else {
        fallback_title.to_string()
    };

    Some(JoinableStreamDto {
        virtual_id: caller_vid,
        title,
        logo: item.header.logo.to_string(),
        category: item.header.group.to_string(),
        protocol: playback_kind_protocol(kind).to_string(),
        requires_free_slot,
    })
}

/// Lists running shared streams that the authenticated caller can join without consuming a new provider connection.
pub async fn get_me_streams_joinable(State(app_state): State<Arc<AppState>>, api_user: ApiUserContext) -> Response {
    if !api_user.user.player_features.joinable_streams {
        return (StatusCode::FORBIDDEN, "feature_disabled").into_response();
    }

    let active_connections = app_state.active_users.user_connections(&api_user.user.username).await;
    let free_joins_in_use = app_state.shared_stream_manager.user_free_shared_joins(&api_user.user.username).await;
    let requires_free_slot = caller_needs_free_slot(&api_user.user, active_connections, free_joins_in_use);

    let streams = app_state.active_users.panel_streams().await;
    // Only a listing that contains a single-source stream of another target needs the caller's
    // published UUID index; same-target listings never read it.
    let classified_streams: Vec<(&_, Option<UUIDType>)> = streams
        .iter()
        .map(|s| {
            let is_foreign =
                s.channel.shared && s.channel.target_id != api_user.target.id && s.channel.group_virtual_id.is_none();
            let uuid = if is_foreign { Some(channel_provider_uuid(&s.channel)) } else { None };
            (s, uuid)
        })
        .collect();
    let foreign_uuids: Vec<UUIDType> = classified_streams.iter().filter_map(|(_, u)| *u).collect();

    let foreign_vids: std::collections::HashMap<UUIDType, u32> = if foreign_uuids.is_empty() {
        std::collections::HashMap::new()
    } else if let Some(mut index) = TargetUuidIndex::open(&app_state.app_config, &api_user.target).await {
        tokio::task::spawn_blocking(move || {
            let mut map = std::collections::HashMap::new();
            for uuid in foreign_uuids {
                if let Some(vid) = index.virtual_id(&uuid) {
                    map.insert(uuid, vid);
                }
            }
            map
        })
        .await
        .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

    let mut seen_keys = std::collections::HashSet::new();
    let mut joinable = Vec::new();

    for (s, foreign_uuid) in classified_streams {
        if !s.channel.shared {
            continue;
        }

        let cross_target = s.channel.target_id != api_user.target.id;
        let caller_vid = if cross_target {
            // Group keys are target-scoped, so the caller's group playback cannot join this stream.
            if s.channel.group_virtual_id.is_some() {
                continue;
            }
            let Some(uuid) = foreign_uuid else { continue };
            let Some(&vid) = foreign_vids.get(&uuid) else { continue };
            let resolved =
                classify_virtual_id(&app_state.app_config, &app_state.playlists, &api_user.target, VirtualId::new(vid))
                    .await;
            // A caller id that replays through a group keeps its target-scoped group key.
            if !matches!(resolved, ResolvedStreamTarget::Item) {
                continue;
            }
            vid
        } else {
            s.channel.client_virtual_id()
        };

        let kind = PlaybackKind::classify(s.channel.item_type, extract_extension_from_url(&s.channel.url));
        let protocol = playback_kind_protocol(kind);
        if seen_keys.contains(&(caller_vid, protocol)) {
            continue;
        }

        if let Some(dto) =
            build_joinable_entry(&app_state, &api_user, &s.channel, caller_vid, kind, cross_target, requires_free_slot)
                .await
        {
            seen_keys.insert((caller_vid, protocol));
            joinable.push(dto);
        }
    }

    Json(joinable).into_response()
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct MeChannelsQuery {
    pub query: Option<String>,
    pub group: Option<String>,
    pub limit: Option<usize>,
}

/// Lists live channels of the caller's playlist that pass their content filter.
pub async fn get_me_channels(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Query(params): Query<MeChannelsQuery>,
) -> Result<Json<Vec<shared::model::UserChannelDto>>, (StatusCode, &'static str)> {
    if !api_user.user.allows_cluster(shared::model::XtreamCluster::Live) {
        return Ok(Json(Vec::new()));
    }
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let channels = crate::api::endpoints::v1_api_playlist::items::query_target_user_live_channels(
        &app_state,
        &api_user.target,
        &api_user.user,
        params.query.as_deref(),
        params.group.as_deref(),
        limit,
    )
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "failed_reading_channels"))?;
    Ok(Json(channels))
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct MeEpgQuery {
    pub vid: u32,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Programmes of one visible channel in `[from, to]` with timeshift applied.
pub async fn get_me_epg(
    State(app_state): State<Arc<AppState>>,
    api_user: ApiUserContext,
    Query(params): Query<MeEpgQuery>,
) -> Result<Json<Vec<shared::model::UserProgrammeDto>>, (StatusCode, &'static str)> {
    if !api_user.user.allows_cluster(shared::model::XtreamCluster::Live) {
        return Err((StatusCode::NOT_FOUND, "channel_not_found"));
    }
    let now = chrono::Utc::now().timestamp();
    let from_ts = match params.from.as_deref() {
        Some(val) if !val.trim().is_empty() => {
            crate::api::endpoints::v1_api_playlist::epg_inspect::parse_instant_ts(val)
                .ok_or((StatusCode::BAD_REQUEST, "invalid_from_timestamp"))?
        }
        _ => now,
    };
    let to_ts = match params.to.as_deref() {
        Some(val) if !val.trim().is_empty() => {
            crate::api::endpoints::v1_api_playlist::epg_inspect::parse_instant_ts(val)
                .ok_or((StatusCode::BAD_REQUEST, "invalid_to_timestamp"))?
        }
        _ => from_ts.saturating_add(24 * 3600),
    };
    if to_ts <= from_ts {
        return Err((StatusCode::BAD_REQUEST, "to_must_be_after_from"));
    }
    if to_ts.saturating_sub(from_ts) > 48 * 3600 {
        return Err((StatusCode::BAD_REQUEST, "window_exceeds_48_hours"));
    }

    let Some(channel) = crate::api::endpoints::v1_api_playlist::items::find_target_user_live_channel(
        &app_state,
        &api_user.target,
        &api_user.user,
        params.vid,
    )
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "failed_reading_channel"))?
    else {
        return Err((StatusCode::NOT_FOUND, "channel_not_found"));
    };

    let Some(epg_channel_id) = channel.epg_channel_id.as_deref() else {
        return Ok(Json(Vec::new()));
    };

    let programmes = crate::api::endpoints::v1_api_playlist::epg_inspect::query_channel_epg_programmes(
        &app_state,
        &api_user.target,
        epg_channel_id,
        from_ts,
        to_ts,
    )
    .await
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "failed_reading_epg"))?
    .unwrap_or_default();

    let timeshift = tuliprox_core::utils::epg_parser::parse_timeshift(api_user.user.epg_timeshift.as_deref());

    let dtos: Vec<shared::model::UserProgrammeDto> = programmes
        .into_iter()
        .map(|p| shared::model::UserProgrammeDto {
            start: tuliprox_core::utils::epg_parser::format_iso_time_with_timeshift(p.start, &timeshift),
            start_ts: p.start,
            stop: tuliprox_core::utils::epg_parser::format_iso_time_with_timeshift(p.stop, &timeshift),
            stop_ts: p.stop,
            title: p.title.as_deref().unwrap_or_default().to_string(),
            desc: p.desc.as_deref().map(ToString::to_string),
            categories: p.categories.into_iter().map(|c| c.value.to_string()).collect(),
            is_live: p.is_live,
            is_new: p.is_new,
        })
        .collect();

    Ok(Json(dtos))
}

/// Registers the `/me` player API routes on the application router.
pub fn me_player_api_register(web_ui_path: &str) -> axum::Router<Arc<AppState>> {
    axum::Router::new().nest(
        &concat_path_leading_slash(web_ui_path, "/api/v1/me"),
        axum::Router::new()
            .route("/capabilities", axum::routing::get(get_me_capabilities))
            .route("/channels", axum::routing::get(get_me_channels))
            .route("/epg", axum::routing::get(get_me_epg))
            .route("/streams", axum::routing::get(get_me_streams))
            .route("/streams/joinable", axum::routing::get(get_me_streams_joinable))
            .route("/streams/{uid}", axum::routing::delete(delete_me_stream))
            .route("/streams/{uid}/failover-members", axum::routing::get(get_me_stream_failover_members))
            .route("/streams/{uid}/failover-switch", axum::routing::post(post_me_stream_failover_switch))
            .route("/streams/{uid}/split", axum::routing::post(post_me_stream_split))
            .route("/channels/{vid}/failover-members", axum::routing::get(get_me_channel_failover_members))
            .route("/events", axum::routing::get(super::me_events_api::get_me_events))
            .merge(super::me_progress_api::me_progress_api_router()),
    )
}

#[cfg(test)]
#[allow(clippy::similar_names, clippy::too_many_lines, clippy::too_many_arguments, clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::{
        api::{
            endpoints::v1_api::v1_api_register,
            model::{create_test_app_state, ConnectionKind, ConnectionParams},
        },
        auth::{create_jwt_api_user, create_jwt_web_user, Fingerprint},
        model::{ApiProxyConfig, Config, ConfigInput, ConfigProvider, ConfigSource, ConfigTarget, SourcesConfig},
    };
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use shared::model::{
        ApiProxyConfigDto, ClusterFlags, FailoverGroupOrigin, FailoverGroupPolicy, InputType, JoinableStreamDto,
        M3uPlaylistItem, Permission, PlayerCapabilitiesDto, PlayerFeaturesDto, PlayerRecordingLevel, PlaylistItemType,
        ProxyUserCredentialsDto, StreamChannel, TargetUserDto, UUIDType, UserConnectionPermission,
        UserFailoverMembersDto, UserId, UserStreamDto, VirtualId, WebAuthConfigDto, WebUiConfigDto, XtreamCluster,
    };
    use std::{net::SocketAddr, sync::Arc, time::Duration};
    use tempfile::TempDir;
    use tower::ServiceExt;
    use tuliprox_repository::{
        bplustree::BPlusTree,
        failover_group_repository::{FailoverGroupRecord, FailoverGroupStore, GroupMember},
        get_target_storage_path,
        target_stream_item::member_to_m3u_item,
        TargetPlaylistStorage,
    };
    use tuliprox_session::active_user_manager::CreateUserSessionParams;

    /// Test routers get a loopback peer address, as a real serve loop would provide.
    fn mock_peer() -> axum::extract::connect_info::MockConnectInfo<SocketAddr> {
        axum::extract::connect_info::MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345)))
    }

    /// Every test router is built from this registration; the wrapper installs the peer
    /// address once instead of annotating each test's router.
    fn me_player_api_register(web_ui_path: &str) -> axum::Router<Arc<AppState>> {
        super::me_player_api_register(web_ui_path).layer(mock_peer())
    }

    struct TestEnv {
        _dir: TempDir,
        app_state: Arc<AppState>,
        alice_token: String,
        bob_token: String,
        carol_token: String,
        dave_token: String,
        eve_token: String,
        admin_token: String,
    }

    fn setup_test_env() -> TestEnv {
        let dir = TempDir::new().unwrap();
        let web_auth_dto = WebAuthConfigDto {
            enabled: true,
            issuer: "test".to_string(),
            secret: "test-secret".to_string(),
            ..Default::default()
        };
        let web_ui_dto = WebUiConfigDto {
            enabled: true,
            auth: Some(web_auth_dto.clone()),
            user_ui_enabled: true,
            ..Default::default()
        };
        let config = Config {
            storage_dir: dir.path().to_string_lossy().to_string(),
            web_ui: Some((&web_ui_dto).into()),
            ..Default::default()
        };
        let app_state = create_test_app_state(config);

        let target = Arc::new(ConfigTarget {
            managed_channels: Vec::new(),
            curation: None,
            id: 1,
            enabled: true,
            name: "main".into(),
            options: Some(shared::model::ConfigTargetOptions {
                failover_groups: Some(shared::model::FailoverGroupsTargetConfig {
                    enabled: true,
                    ..Default::default()
                }),
                share_live_streams: shared::model::ConfigTargetShareLiveStreams { hls: true, mpeg_ts: true },
                ..Default::default()
            }),
            sort: None,
            filter: tuliprox_core::model::StagedFilter::default(),
            output: vec![
                tuliprox_core::model::TargetOutput::Xtream(tuliprox_core::model::XtreamTargetOutput::from(
                    &shared::model::XtreamTargetOutputDto::default(),
                )),
                tuliprox_core::model::TargetOutput::M3u(tuliprox_core::model::M3uTargetOutput::from(
                    &shared::model::M3uTargetOutputDto::default(),
                )),
            ],
            rename: None,
            mapping_ids: None,
            mapping: Arc::default(),
            favourites: None,
            processing_order: shared::model::ProcessingOrder::default(),
            execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
            watch: None,
            use_memory_cache: false,
        });
        let sources = SourcesConfig {
            inputs: Vec::new(),
            sources: vec![ConfigSource { inputs: Vec::new(), targets: vec![Arc::clone(&target)] }],
            ..Default::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let api_proxy_dto = ApiProxyConfigDto {
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![
                    ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 5,
                        filter: Some(r#"Group = "News""#.to_string()),
                        player_features: Some(PlayerFeaturesDto {
                            source_switch: Some(true),
                            joinable_streams: Some(true),
                            recording: Some(PlayerRecordingLevel::Private),
                            recording_quota: Some(shared::model::ByteSize::new("1024")),
                            max_concurrent_recordings: Some(2),
                            max_recording_rules: Some(15),
                            free_shared_joins: Some(true),
                            max_free_shared_joins: Some(3),
                            watch_progress: Some(true),
                            event_stream: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "bob".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 2,
                        player_features: Some(PlayerFeaturesDto {
                            source_switch: Some(true),
                            joinable_streams: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 2,
                        player_features: Some(PlayerFeaturesDto {
                            source_switch: Some(false),
                            joinable_streams: Some(false),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "dave".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 2,
                        player_features: Some(PlayerFeaturesDto {
                            source_switch: Some(true),
                            recording: Some(PlayerRecordingLevel::Read),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "eve".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 2,
                        player_features: Some(PlayerFeaturesDto {
                            source_switch: Some(false),
                            recording: Some(PlayerRecordingLevel::Shared),
                            max_recording_rules: Some(25),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let mut api_proxy = ApiProxyConfig::from(&api_proxy_dto);
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).unwrap();

        let web_auth = app_state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();
        let alice_token = create_jwt_api_user(&web_auth, "alice", UserId::api("alice")).unwrap();
        let bob_token = create_jwt_api_user(&web_auth, "bob", UserId::api("bob")).unwrap();
        let carol_token = create_jwt_api_user(&web_auth, "carol", UserId::api("carol")).unwrap();
        let dave_token = create_jwt_api_user(&web_auth, "dave", UserId::api("dave")).unwrap();
        let eve_token = create_jwt_api_user(&web_auth, "eve", UserId::api("eve")).unwrap();
        let admin_token =
            create_jwt_web_user(&web_auth, "admin", Permission::UserWrite.into(), 0, UserId::web("admin")).unwrap();

        TestEnv { _dir: dir, app_state, alice_token, bob_token, carol_token, dave_token, eve_token, admin_token }
    }

    async fn register_stream(
        app_state: &AppState,
        meter_uid: u32,
        username: &str,
        virtual_id: u32,
        group_virtual_id: Option<u32>,
        item_type: PlaylistItemType,
        shared: bool,
        session_token: Option<String>,
    ) -> u32 {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let fp = Fingerprint::new("fp".into(), "127.0.0.1".into(), addr);
        let channel = StreamChannel {
            target_id: 1,
            virtual_id,
            group_virtual_id,
            provider_id: 1,
            input_name: "test-in".into(),
            item_type,
            cluster: XtreamCluster::Live,
            group: "test-group".into(),
            title: "Test Stream".into(),
            url: "http://example.com/stream.ts".into(),
            shared,
            shared_joined_existing: shared.then_some(true),
            ..Default::default()
        };
        let connection = ConnectionParams {
            meter_uid,
            username,
            max_connections: 5,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp,
            provider: "test-prov".into(),
            stream_channel: &channel,
            user_agent: "test-ua".into(),
            session_token: session_token.as_deref(),
        };
        let _ = app_state.connection_manager.update_connection(connection).await;
        let streams = app_state.active_users.user_streams(username).await;
        streams.into_iter().find(|s| s.channel.virtual_id == virtual_id).map(|s| s.uid).expect("stream registered")
    }

    fn seed_group(app_state: &AppState, group_vid: u32, member_vids: &[u32]) -> FailoverGroupRecord {
        let members = member_vids
            .iter()
            .enumerate()
            .map(|(idx, &m_vid)| GroupMember {
                member_vid: VirtualId::new(m_vid),
                input_name: format!("input_{idx}").into(),
                provider_id: idx as u32 + 1,
                input_stream_id: format!("stream_{idx}").into(),
                url: format!("http://example.com/{m_vid}.ts").into(),
                item_type: PlaylistItemType::Live,
                cluster: XtreamCluster::Live,
                title: format!("Member {idx}").into(),
                archive: None,
                epg_channel_id: None,
                additional_properties: None,
                input_url: Arc::from(""),
                upstream_user_agent: None,
            })
            .collect();

        let record = FailoverGroupRecord {
            group_vid: VirtualId::new(group_vid),
            key: "test_group".into(),
            origin: FailoverGroupOrigin::Auto,
            policy: FailoverGroupPolicy::Auto,
            members,
            orphaned: Vec::new(),
        };

        let storage = get_target_storage_path(&app_state.app_config.config.load(), "main").unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        FailoverGroupStore::write(&storage, std::slice::from_ref(&record), &[], None).unwrap();
        app_state.playlists.bump_failover_group_generation("main");

        record
    }

    async fn seed_m3u_group_item(app_state: &AppState, group: &FailoverGroupRecord, group_name: &str, title: &str) {
        let mut tree = BPlusTree::new();
        let mut item = member_to_m3u_item(group, 0);
        item.virtual_id = group.group_vid;
        item.group = Arc::from(group_name);
        item.name = Arc::from(title);
        item.title = Arc::from(title);
        tree.insert(group.group_vid.get(), item);
        app_state
            .playlists
            .data
            .write()
            .await
            .insert("main".to_string(), TargetPlaylistStorage { m3u: Some(tree), xtream: None, id_mapping: None });
    }

    async fn seed_test_m3u_items(app_state: &AppState, items: &[(u32, &str, &str, &str)]) {
        let mut tree = BPlusTree::new();
        for &(vid, group_name, title, logo) in items {
            let group = seed_group(app_state, vid, &[vid + 1]);
            let mut item = member_to_m3u_item(&group, 0);
            item.virtual_id = VirtualId::new(vid);
            item.group = Arc::from(group_name);
            item.name = Arc::from(title);
            item.title = Arc::from(title);
            item.logo = Arc::from(logo);
            tree.insert(vid, item);
        }
        app_state
            .playlists
            .data
            .write()
            .await
            .insert("main".to_string(), TargetPlaylistStorage { m3u: Some(tree), xtream: None, id_mapping: None });
    }

    /// A caller-target channel that also exists as a stream in another target.
    struct CallerChannelFixture {
        vid: u32,
        input_name: &'static str,
        provider_id: u32,
        url: &'static str,
        group: &'static str,
        title: &'static str,
        logo: &'static str,
    }

    impl CallerChannelFixture {
        /// The provider identity the target's UUID index persists for this channel.
        fn uuid(&self) -> UUIDType {
            generate_provider_playlist_uuid(self.input_name, &self.provider_id.to_string(), PlaylistItemType::Live)
        }

        fn item(&self) -> M3uPlaylistItem {
            let record = FailoverGroupRecord {
                group_vid: VirtualId::new(0),
                key: "caller_fixture".into(),
                origin: FailoverGroupOrigin::Auto,
                policy: FailoverGroupPolicy::Auto,
                members: vec![GroupMember {
                    member_vid: VirtualId::new(self.vid),
                    input_name: self.input_name.into(),
                    provider_id: self.provider_id,
                    input_stream_id: self.provider_id.to_string().into(),
                    url: self.url.into(),
                    item_type: PlaylistItemType::Live,
                    cluster: XtreamCluster::Live,
                    title: self.title.into(),
                    archive: None,
                    epg_channel_id: None,
                    additional_properties: None,
                    input_url: Arc::from(""),
                    upstream_user_agent: None,
                }],
                orphaned: Vec::new(),
            };
            let mut item = member_to_m3u_item(&record, 0);
            item.virtual_id = VirtualId::new(self.vid);
            item.group = Arc::from(self.group);
            item.logo = Arc::from(self.logo);
            item
        }

        /// A single-source shared stream of another target carrying this channel's provider identity.
        fn foreign_channel(&self, target_id: u16, virtual_id: u32) -> StreamChannel {
            StreamChannel {
                target_id,
                virtual_id,
                group_virtual_id: None,
                provider_id: self.provider_id,
                input_name: self.input_name.into(),
                item_type: PlaylistItemType::Live,
                cluster: XtreamCluster::Live,
                group: "foreign-group".into(),
                title: "Foreign Title".into(),
                url: self.url.into(),
                shared: true,
                shared_joined_existing: Some(true),
                ..Default::default()
            }
        }
    }

    /// Seeds caller-target channels without failover groups, so their ids stay plain items.
    async fn seed_caller_channels(app_state: &AppState, fixtures: &[CallerChannelFixture]) {
        let mut tree = BPlusTree::new();
        for fixture in fixtures {
            tree.insert(fixture.vid, fixture.item());
        }
        app_state
            .playlists
            .data
            .write()
            .await
            .insert("main".to_string(), TargetPlaylistStorage { m3u: Some(tree), xtream: None, id_mapping: None });
    }

    /// Writes the published UUID index a target's playlist persistence leaves behind.
    fn seed_target_uuid_index(app_state: &AppState, target_name: &str, entries: &[(UUIDType, u32)]) {
        let storage = get_target_storage_path(&app_state.app_config.config.load(), target_name).unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        let index_path = tuliprox_repository::get_target_id_mapping_file(&storage).with_extension("uuid.db");
        let mut tree = BPlusTree::<UUIDType, u32>::new();
        for &(uuid, vid) in entries {
            tree.insert(uuid, vid);
        }
        tree.store(&index_path).unwrap();
    }

    /// Rebuilds the configured targets so the caller's target stops joining shared streams.
    fn disable_target_sharing(app_state: &AppState) {
        let sources = app_state.app_config.sources.load();
        let mut next = (**sources).clone();
        for source in &mut next.sources {
            for target in &mut source.targets {
                let mut target_config = (**target).clone();
                if let Some(options) = target_config.options.as_mut() {
                    options.share_live_streams = shared::model::ConfigTargetShareLiveStreams::default();
                }
                *target = Arc::new(target_config);
            }
        }
        app_state.app_config.sources.store(Arc::new(next));
    }

    async fn register_stream_full(
        app_state: &AppState,
        meter_uid: u32,
        username: &str,
        target_id: u16,
        virtual_id: u32,
        group_virtual_id: Option<u32>,
        shared_stream_id: Option<u64>,
        item_type: PlaylistItemType,
        shared: bool,
        session_token: Option<String>,
        url: Option<&str>,
    ) -> u32 {
        let channel = StreamChannel {
            target_id,
            virtual_id,
            group_virtual_id,
            provider_id: 1,
            input_name: "test-in".into(),
            item_type,
            cluster: XtreamCluster::Live,
            group: "test-group".into(),
            title: "Test Stream".into(),
            url: url.unwrap_or("http://example.com/stream.ts").into(),
            shared,
            shared_joined_existing: shared.then_some(true),
            shared_stream_id,
            ..Default::default()
        };
        register_stream_channel(app_state, meter_uid, username, &channel, session_token).await
    }

    async fn register_stream_channel(
        app_state: &AppState,
        meter_uid: u32,
        username: &str,
        channel: &StreamChannel,
        session_token: Option<String>,
    ) -> u32 {
        let port = u16::try_from(8080 + meter_uid).unwrap_or(8080);
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let fp = Fingerprint::new(format!("fp-{meter_uid}"), "127.0.0.1".into(), addr);
        let virtual_id = channel.virtual_id;
        let connection = ConnectionParams {
            meter_uid,
            username,
            max_connections: 5,
            soft_connections: 0,
            connection_kind: ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &fp,
            provider: "test-prov".into(),
            stream_channel: channel,
            user_agent: "test-ua".into(),
            session_token: session_token.as_deref(),
        };
        let _ = app_state.connection_manager.update_connection(connection).await;
        let streams = app_state.active_users.user_streams(username).await;
        streams
            .into_iter()
            .find(|s| s.channel.virtual_id == virtual_id && s.meter_uid == meter_uid)
            .map(|s| s.uid)
            .expect("stream registered")
    }

    async fn register_stream_ext(
        app_state: &AppState,
        meter_uid: u32,
        username: &str,
        target_id: u16,
        virtual_id: u32,
        group_virtual_id: Option<u32>,
        shared_stream_id: Option<u64>,
        item_type: PlaylistItemType,
        shared: bool,
        session_token: Option<String>,
    ) -> u32 {
        register_stream_full(
            app_state,
            meter_uid,
            username,
            target_id,
            virtual_id,
            group_virtual_id,
            shared_stream_id,
            item_type,
            shared,
            session_token,
            None,
        )
        .await
    }

    #[test]
    fn test_user_failover_dto_contains_no_provider_internals() {
        let dto = shared::model::UserFailoverMembersDto {
            group_name: "Group 1".to_string(),
            current_member_vid: Some(10),
            members: vec![shared::model::UserFailoverMemberDto {
                member_vid: 10,
                index: 0,
                title: "Member 0".to_string(),
                has_archive: false,
                current: true,
                health: Some(shared::model::UserMemberHealthDto {
                    state: shared::model::UserMemberHealthState::Degraded,
                    resolution: Some(shared::model::VideoResolution::P1080),
                    video_codec: Some(shared::model::VideoCodec::H264),
                    bitrate_kbps: Some(5000),
                    qos_24h: Some(90),
                    qos_7d: Some(85),
                    retry_after_secs: None,
                }),
            }],
        };
        let json_str = serde_json::to_string(&dto).unwrap();
        assert!(!json_str.contains("input_name"));
        assert!(!json_str.contains("provider_id"));
        assert!(!json_str.contains("input_stream_id"));
        assert!(!json_str.contains("member_ref"));
        assert!(!json_str.contains("url"));
        assert!(!json_str.contains("user_agent"));
        assert!(!json_str.contains("slots"));
        assert!(!json_str.contains("input_circuit"));
        assert!(!json_str.contains("rank"));
        assert!(!json_str.contains("score"));
        assert!(json_str.contains("group_name"));
        assert!(json_str.contains("current_member_vid"));
        assert!(json_str.contains("member_vid"));
        assert!(json_str.contains("has_archive"));
        assert!(json_str.contains("current"));
        assert!(json_str.contains("health"));
        assert!(json_str.contains("state"));
        assert!(json_str.contains("degraded"));
        assert!(json_str.contains("P1080"));
        assert!(json_str.contains("H264"));
        assert!(json_str.contains("5000"));
        assert!(json_str.contains("90"));
        assert!(json_str.contains("85"));
    }

    #[tokio::test]
    async fn test_me_channel_failover_members_health_seeded_and_unseeded() {
        use tuliprox_session::{QosKey, QosPart};

        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let group = seed_group(&env.app_state, 1000, &[1001, 1002]);
        seed_m3u_group_item(&env.app_state, &group, "News", "Group Channel News").await;

        env.app_state.member_health.replace_qos([(
            QosKey::Provider { input: "input_0".into(), provider_id: 1 },
            QosPart { score_24h: 92, score_7d: 88, confidence: 100, failure_streak: 0 },
        )]);
        env.app_state.member_health.patch_probe(
            &(Arc::from("input_0"), Arc::from("stream_0")),
            Some(r#"{"codec_name":"h264","width":1920,"height":1080}"#),
            5500,
        );

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels/1000/failover-members")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let raw_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        let member_0 = &raw_json["members"][0];
        assert_eq!(member_0["member_vid"], 1001);
        assert_eq!(
            member_0["health"],
            serde_json::json!({
                "state": "ok",
                "resolution": "P1080",
                "video_codec": "H264",
                "bitrate_kbps": 5500,
                "qos_24h": 92,
                "qos_7d": 88
            })
        );

        let member_1 = &raw_json["members"][1];
        assert_eq!(member_1["member_vid"], 1002);
        assert_eq!(
            member_1["health"],
            serde_json::json!({
                "state": "ok"
            })
        );
    }

    #[tokio::test]
    async fn test_me_capabilities() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Add 1 shared stream for alice
        let _ = register_stream(&env.app_state, 1, "alice", 100, None, PlaylistItemType::Live, true, None).await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let cap: PlayerCapabilitiesDto = serde_json::from_slice(&body).unwrap();
        assert!(cap.source_switch);
        assert!(cap.joinable_streams);
        assert_eq!(cap.recording, PlayerRecordingLevel::Private);
        assert_eq!(cap.recording_quota_bytes, Some(1024));
        assert_eq!(cap.max_concurrent_recordings, Some(2));
        assert_eq!(cap.max_recording_rules, Some(15));
        assert!(cap.free_shared_joins);
        assert_eq!(cap.max_free_shared_joins, 3);
        assert!(cap.watch_progress);
        assert!(!cap.event_stream);
        assert!(cap.stream_split);
        assert_eq!(cap.max_connections, 5);
        assert_eq!(cap.active_connections, 1);
        assert_eq!(cap.shared_joins, 1);
        assert_eq!(cap.free_shared_joins_in_use, 0);
        assert_eq!(cap.source_switch_protocols, vec!["ts".to_string(), "hls".to_string()]);
        assert!(cap.effective_permissions.contains(&"recording.read".to_string()));
    }

    #[tokio::test]
    async fn test_me_capabilities_mapping_rules_and_split() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Bob has source_switch: true, recording: None (inherited default)
        let req_bob = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res_bob = router.clone().oneshot(req_bob).await.unwrap();
        assert_eq!(res_bob.status(), StatusCode::OK);
        let cap_bob: PlayerCapabilitiesDto =
            serde_json::from_slice(&to_bytes(res_bob.into_body(), 4096).await.unwrap()).unwrap();
        assert!(cap_bob.source_switch);
        assert!(cap_bob.stream_split);
        assert_eq!(cap_bob.stream_split, cap_bob.source_switch);
        assert_eq!(cap_bob.max_recording_rules, None);

        // Carol has source_switch: false, recording: None
        let req_carol = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.carol_token))
            .body(Body::empty())
            .unwrap();
        let res_carol = router.clone().oneshot(req_carol).await.unwrap();
        assert_eq!(res_carol.status(), StatusCode::OK);
        let cap_carol: PlayerCapabilitiesDto =
            serde_json::from_slice(&to_bytes(res_carol.into_body(), 4096).await.unwrap()).unwrap();
        assert!(!cap_carol.source_switch);
        assert!(!cap_carol.stream_split);
        assert_eq!(cap_carol.stream_split, cap_carol.source_switch);
        assert_eq!(cap_carol.max_recording_rules, None);

        // Dave has source_switch: true, recording: Read -> max_recording_rules must be None
        let req_dave = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.dave_token))
            .body(Body::empty())
            .unwrap();
        let res_dave = router.clone().oneshot(req_dave).await.unwrap();
        assert_eq!(res_dave.status(), StatusCode::OK);
        let cap_dave: PlayerCapabilitiesDto =
            serde_json::from_slice(&to_bytes(res_dave.into_body(), 4096).await.unwrap()).unwrap();
        assert!(cap_dave.source_switch);
        assert!(cap_dave.stream_split);
        assert_eq!(cap_dave.recording, PlayerRecordingLevel::Read);
        assert_eq!(cap_dave.max_recording_rules, None);

        // Eve has source_switch: false, recording: Shared, max_recording_rules: 25
        let req_eve = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.eve_token))
            .body(Body::empty())
            .unwrap();
        let res_eve = router.oneshot(req_eve).await.unwrap();
        assert_eq!(res_eve.status(), StatusCode::OK);
        let cap_eve: PlayerCapabilitiesDto =
            serde_json::from_slice(&to_bytes(res_eve.into_body(), 4096).await.unwrap()).unwrap();
        assert!(!cap_eve.source_switch);
        assert!(!cap_eve.stream_split);
        assert_eq!(cap_eve.recording, PlayerRecordingLevel::Shared);
        assert_eq!(cap_eve.max_recording_rules, Some(25));
    }

    #[tokio::test]
    async fn test_me_streams_returns_only_caller_streams() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let uid_alice =
            register_stream(&env.app_state, 1, "alice", 101, None, PlaylistItemType::Live, false, None).await;
        let uid_alice_dash =
            register_stream(&env.app_state, 3, "alice", 103, None, PlaylistItemType::LiveDash, false, None).await;
        let uid_alice_hls =
            register_stream(&env.app_state, 4, "alice", 104, None, PlaylistItemType::LiveHls, false, None).await;
        let uid_bob = register_stream(&env.app_state, 2, "bob", 102, None, PlaylistItemType::Live, false, None).await;

        let req_alice = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_alice = router.clone().oneshot(req_alice).await.unwrap();
        assert_eq!(res_alice.status(), StatusCode::OK);
        let body = to_bytes(res_alice.into_body(), 4096).await.unwrap();
        let streams_alice: Vec<UserStreamDto> = serde_json::from_slice(&body).unwrap();
        assert_eq!(streams_alice.len(), 3);
        let s_ts = streams_alice.iter().find(|s| s.uid == uid_alice).unwrap();
        assert_eq!(s_ts.virtual_id, 101);
        assert_eq!(s_ts.protocol, "ts");
        let s_dash = streams_alice.iter().find(|s| s.uid == uid_alice_dash).unwrap();
        assert_eq!(s_dash.virtual_id, 103);
        assert_eq!(s_dash.protocol, "dash");
        let s_hls = streams_alice.iter().find(|s| s.uid == uid_alice_hls).unwrap();
        assert_eq!(s_hls.virtual_id, 104);
        assert_eq!(s_hls.protocol, "hls");

        let req_bob = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res_bob = router.oneshot(req_bob).await.unwrap();
        assert_eq!(res_bob.status(), StatusCode::OK);
        let body = to_bytes(res_bob.into_body(), 4096).await.unwrap();
        let streams_bob: Vec<UserStreamDto> = serde_json::from_slice(&body).unwrap();
        assert_eq!(streams_bob.len(), 1);
        assert_eq!(streams_bob[0].uid, uid_bob);
        assert_eq!(streams_bob[0].virtual_id, 102);
    }

    #[tokio::test]
    async fn test_delete_own_stream_success_and_foreign_not_found() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let uid_alice =
            register_stream(&env.app_state, 1, "alice", 101, None, PlaylistItemType::Live, false, None).await;
        assert_eq!(env.app_state.active_users.user_connections("alice").await, 1);

        // Bob tries to delete Alice's stream -> 404
        let req_bob = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/me/streams/{uid_alice}"))
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res_bob = router.clone().oneshot(req_bob).await.unwrap();
        assert_eq!(res_bob.status(), StatusCode::NOT_FOUND);

        // Alice tries to delete nonexistent stream -> 404
        let req_nonexistent = Request::builder()
            .method("DELETE")
            .uri("/api/v1/me/streams/99999")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_nonexistent = router.clone().oneshot(req_nonexistent).await.unwrap();
        assert_eq!(res_nonexistent.status(), StatusCode::NOT_FOUND);

        // Alice deletes own stream -> 204
        let req_alice = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/me/streams/{uid_alice}"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_alice = router.oneshot(req_alice).await.unwrap();
        assert_eq!(res_alice.status(), StatusCode::NO_CONTENT);

        // Stream gone and connections decremented
        assert!(env.app_state.active_users.find_panel_stream_by_uid(uid_alice).await.is_none());
        assert_eq!(env.app_state.active_users.user_connections("alice").await, 0);
    }

    #[tokio::test]
    async fn test_delete_own_stream_emits_stream_ended_stopped() {
        use shared::model::StreamEndedReason;

        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let uid_alice =
            register_stream(&env.app_state, 1, "alice", 101, None, PlaylistItemType::Live, false, None).await;

        let streams = env.app_state.active_users.user_streams("alice").await;
        let stream = streams.into_iter().find(|s| s.uid == uid_alice).expect("stream found");
        let _close_rx = env.app_state.connection_manager.register_close_socket(stream.addr);

        let mut rx = env.app_state.event_manager.get_event_channel();

        let req_alice = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/me/streams/{uid_alice}"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_alice = router.oneshot(req_alice).await.unwrap();
        assert_eq!(res_alice.status(), StatusCode::NO_CONTENT);

        let reason = loop {
            let event = tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .expect("timed out waiting for event")
                .unwrap();
            if let shared::model::EventMessage::StreamEnded { uid, reason, .. } = event {
                if uid == uid_alice {
                    break reason;
                }
            }
        };
        assert_eq!(reason, StreamEndedReason::Stopped);
    }

    #[tokio::test]
    async fn test_delete_stream_with_session_token_terminates_session() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let user = env.app_state.app_config.get_user_credentials("alice").unwrap();
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let token = env
            .app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &user,
                session_token: "sess-alice-delete-test",
                virtual_id: 101,
                provider: "prov",
                stream_url: "http://example.com/101.ts",
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        assert!(env.app_state.active_users.get_and_update_user_session("alice", &token).await.is_some());

        let uid =
            register_stream(&env.app_state, 1, "alice", 101, None, PlaylistItemType::Live, false, Some(token.clone()))
                .await;

        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/v1/me/streams/{uid}"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        assert!(env.app_state.active_users.get_and_update_user_session("alice", &token).await.is_none());
    }

    #[tokio::test]
    async fn test_failover_members_feature_disabled_outside_filter_not_a_group() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let group = seed_group(&env.app_state, 1000, &[1001, 1002]);

        // 1. Carol (source_switch: false) -> 403 feature_disabled
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels/1000/failover-members")
            .header("Authorization", format!("Bearer {}", env.carol_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // 2. Channel variant: Not a group (vid 999) -> 404 not found
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels/999/failover-members")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // 3. Channel outside user's filter:
        // Alice has filter Group = "News". We seed group 1000 with group "Sports".
        seed_m3u_group_item(&env.app_state, &group, "Sports", "Group Channel Sports").await;
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels/1000/failover-members")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Now seed group 1000 with group "News" (passes Alice's filter) -> 200 OK
        seed_m3u_group_item(&env.app_state, &group, "News", "Group Channel News").await;
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels/1000/failover-members")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let dto: UserFailoverMembersDto = serde_json::from_slice(&body).unwrap();
        assert_eq!(dto.members.len(), 2);
        assert_eq!(dto.members[0].member_vid, 1001);
        assert_eq!(dto.members[1].member_vid, 1002);

        // 4. Stream variant: own stream vs foreign stream
        let uid_bob = register_stream(&env.app_state, 2, "bob", 1000, None, PlaylistItemType::Live, false, None).await;
        // Alice calls on Bob's stream -> 404
        let req = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/me/streams/{uid_bob}/failover-members"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Bob calls on own stream -> 200
        let req = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/me/streams/{uid_bob}/failover-members"))
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_me_stream_failover_switch_hls_rules() {
        use tuliprox_hls::api::{
            new_hls_access_lease_id, HlsAccessLease, HlsAccessLeaseTiming, HlsPlaybackFamilyKey, HlsSessionKey,
        };

        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let _group = seed_group(&env.app_state, 3000, &[3001, 3002]);

        let uid_alice = register_stream(
            &env.app_state,
            1,
            "alice",
            3000,
            None,
            PlaylistItemType::LiveHls,
            true,
            Some("sess-alice-hls".to_string()),
        )
        .await;
        let uid_bob = register_stream(
            &env.app_state,
            2,
            "bob",
            3000,
            None,
            PlaylistItemType::LiveHls,
            false,
            Some("sess-bob-hls".to_string()),
        )
        .await;

        // Foreign HLS stream of another user -> 404
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_bob}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":3001}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Member not in group -> 400
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":9999}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Set up shared cache HLS session
        let group_key = HlsSessionKey::for_group(1, 3000);
        let session = env.app_state.hls.proxy.sessions().get_or_create_session(group_key, b"secret", 0).await;
        let proxy_session_id = session.read().await.proxy_session_id.clone();

        // 2 user leases bound to this proxy session
        let lease_id1 = new_hls_access_lease_id();
        let lease_id2 = new_hls_access_lease_id();
        let now = tuliprox_core::utils::current_time_millis();
        let lease1 = HlsAccessLease::pending(
            lease_id1.clone(),
            HlsPlaybackFamilyKey::new("alice", "fp-alice"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "sess-alice-hls".to_string(),
            1,
            "3000".to_string(),
            3000,
            now,
            now + 100_000,
        );
        let lease2 = HlsAccessLease::pending(
            lease_id2.clone(),
            HlsPlaybackFamilyKey::new("user2", "fp-user2"),
            proxy_session_id.clone(),
            "user2".to_string(),
            "sess-user2-hls".to_string(),
            1,
            "3000".to_string(),
            3000,
            now,
            now + 100_000,
        );
        env.app_state.hls.proxy.prepare_access_lease(lease1).await;
        env.app_state.hls.proxy.prepare_access_lease(lease2).await;
        env.app_state
            .hls
            .proxy
            .activate_access_lease(
                &lease_id1,
                &proxy_session_id,
                now,
                HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 100_000 },
            )
            .await;
        env.app_state
            .hls
            .proxy
            .activate_access_lease(
                &lease_id2,
                &proxy_session_id,
                now,
                HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 100_000 },
            )
            .await;

        let active_tokens =
            env.app_state.hls.proxy.non_terminal_user_session_tokens_for_proxy_session(&proxy_session_id, now).await;
        assert_eq!(active_tokens.len(), 2);

        // Case 1: Two viewers active -> 409 shared_stream_switch_refused
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":3002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"shared_stream_switch_refused");

        // Case 2: Caller's lease lapsed but other viewer active -> 409 shared_stream_switch_refused
        env.app_state.hls.proxy.hold_access_lease_store_for_test().await.remove_access_lease(&lease_id1);
        let lapsed_lease1 = HlsAccessLease::pending(
            lease_id1.clone(),
            HlsPlaybackFamilyKey::new("alice", "fp-alice"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "sess-alice-hls".to_string(),
            1,
            "3000".to_string(),
            3000,
            now.saturating_sub(20_000),
            now.saturating_sub(10_000),
        );
        env.app_state.hls.proxy.prepare_access_lease(lapsed_lease1).await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":3002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"shared_stream_switch_refused");

        // Case 3: Only caller active -> request reaches the runtime
        env.app_state.hls.proxy.hold_access_lease_store_for_test().await.remove_access_lease(&lease_id2);
        env.app_state.hls.proxy.hold_access_lease_store_for_test().await.remove_access_lease(&lease_id1);
        let active_lease1 = HlsAccessLease::pending(
            lease_id1.clone(),
            HlsPlaybackFamilyKey::new("alice", "fp-alice"),
            proxy_session_id.clone(),
            "alice".to_string(),
            "sess-alice-hls".to_string(),
            1,
            "3000".to_string(),
            3000,
            now,
            now + 100_000,
        );
        env.app_state.hls.proxy.prepare_access_lease(active_lease1).await;
        env.app_state
            .hls
            .proxy
            .activate_access_lease(
                &lease_id1,
                &proxy_session_id,
                now,
                HlsAccessLeaseTiming { active_window_ms: 60_000, valid_window_ms: 100_000 },
            )
            .await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":3002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"origin runtime not available");
    }

    #[tokio::test]
    async fn test_failover_switch_rules() {
        use crate::api::api_utils::failover_group_runtime::SwitchError;

        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let _group = seed_group(&env.app_state, 2000, &[2001, 2002]);

        let uid_alice =
            register_stream(&env.app_state, 1, "alice", 2000, None, PlaylistItemType::Live, true, None).await;
        let uid_bob = register_stream(&env.app_state, 2, "bob", 2000, None, PlaylistItemType::Live, false, None).await;

        // Foreign stream -> 404
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_bob}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2001}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Member not in group -> 400
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":9999}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Shared stream with 2 subscribers -> 409 shared_stream_switch_refused
        let key = crate::api::api_utils::failover_group::group_shared_key(1, 2000);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key, 2).await;
        assert_eq!(env.app_state.shared_stream_manager.subscriber_count(&key).await, 2);

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // Drop 1 subscriber so count == 1
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key, 1).await;
        assert_eq!(env.app_state.shared_stream_manager.subscriber_count(&key).await, 1);

        // Case A: Registry returns InProgress -> 409 switch_in_progress
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _guard = env.app_state.failover_switch_registry.register_shared(1, 2000, tx);

        tokio::spawn(async move {
            if let Some(cmd) = rx.recv().await {
                let _ = cmd.respondent.send(Err(SwitchError::InProgress));
            }
        });

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // Case B: Registry returns Cooldown { retry_after: 2s } -> 429 with Retry-After header
        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let _guard2 = env.app_state.failover_switch_registry.register_shared(1, 2000, tx2);

        tokio::spawn(async move {
            if let Some(cmd) = rx2.recv().await {
                let _ = cmd.respondent.send(Err(SwitchError::Cooldown { retry_after: Duration::from_secs(2) }));
            }
        });

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers().get("retry-after").unwrap(), "2");

        // Case C: Registry returns Ok(()) -> 200 OK
        let (tx3, mut rx3) = tokio::sync::mpsc::unbounded_channel();
        let _guard3 = env.app_state.failover_switch_registry.register_shared(1, 2000, tx3);

        tokio::spawn(async move {
            if let Some(cmd) = rx3.recv().await {
                let _ = cmd.respondent.send(Ok(()));
            }
        });

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2002}"#))
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Case D: the driver owns the cooldown. A second switch right after a successful one is
        // answered by the driver; when the driver accepts it, the handler adds no second cooldown.
        let (tx4, mut rx4) = tokio::sync::mpsc::unbounded_channel();
        let _guard4 = env.app_state.failover_switch_registry.register_shared(1, 2000, tx4);

        tokio::spawn(async move {
            if let Some(cmd) = rx4.recv().await {
                let _ = cmd.respondent.send(Ok(()));
            }
        });

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_alice}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2001}"#))
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_routing_coexistence() {
        let env = setup_test_env();
        let router =
            v1_api_register(true, &env.app_state, "").with_state(Arc::clone(&env.app_state)).layer(mock_peer());

        // 1. /api/v1/me/streams is reachable by API user
        let req_me = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_me = router.clone().oneshot(req_me).await.unwrap();
        assert_eq!(res_me.status(), StatusCode::OK);

        // 2. Admin /api/v1/user/{target} is reachable by admin
        // Sending invalid/empty payload reaches handler and returns BAD_REQUEST (not NOT_FOUND 404)
        let req_admin = Request::builder()
            .method("POST")
            .uri("/api/v1/user/main")
            .header("Authorization", format!("Bearer {}", env.admin_token))
            .header("Content-Type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let res_admin = router.oneshot(req_admin).await.unwrap();
        assert_ne!(res_admin.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_caller_needs_free_slot() {
        let mut user = tuliprox_core::model::ProxyUserCredentials::default();

        // 0 = unlimited -> always false
        user.max_connections = 0;
        assert!(!caller_needs_free_slot(&user, 0, 0));
        assert!(!caller_needs_free_slot(&user, 1, 0));
        assert!(!caller_needs_free_slot(&user, 10, 0));

        // max_connections = 1, free_shared_joins = false
        user.max_connections = 1;
        assert!(!caller_needs_free_slot(&user, 0, 0));
        assert!(caller_needs_free_slot(&user, 1, 0));
        assert!(caller_needs_free_slot(&user, 2, 0));

        // max_connections = 2, free_shared_joins = false
        user.max_connections = 2;
        assert!(!caller_needs_free_slot(&user, 0, 0));
        assert!(!caller_needs_free_slot(&user, 1, 0));
        assert!(caller_needs_free_slot(&user, 2, 0));
        assert!(caller_needs_free_slot(&user, 3, 0));

        // max_connections = 1, free_shared_joins = true, max_free_shared_joins = 1
        user.player_features.free_shared_joins = true;
        user.player_features.max_free_shared_joins = 1;
        user.max_connections = 1;
        assert!(!caller_needs_free_slot(&user, 0, 0));
        assert!(!caller_needs_free_slot(&user, 1, 0));
        assert!(caller_needs_free_slot(&user, 1, 1));
        assert!(caller_needs_free_slot(&user, 1, 2));

        // max_free_shared_joins = 2
        user.player_features.max_free_shared_joins = 2;
        assert!(!caller_needs_free_slot(&user, 1, 0));
        assert!(!caller_needs_free_slot(&user, 1, 1));
        assert!(caller_needs_free_slot(&user, 1, 2));
    }

    #[tokio::test]
    async fn test_joinable_streams_deduplicated_and_ts_protocol() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Channel X: vid 100, group "News", title "News 24", logo "http://logo.png"
        seed_test_m3u_items(&env.app_state, &[(100, "News", "News 24", "http://logo.png")]).await;

        // User A (alice) plays shared channel X
        register_stream_ext(&env.app_state, 1, "alice", 1, 100, None, Some(42), PlaylistItemType::Live, true, None)
            .await;

        // Another user (charlie) also plays shared channel X (same shared_stream_id 42)
        register_stream_ext(&env.app_state, 2, "charlie", 1, 100, None, Some(42), PlaylistItemType::Live, true, None)
            .await;

        // Caller Bob (feature on, target 1, max_connections 2, active 0)
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let list: Vec<JoinableStreamDto> = serde_json::from_slice(&body).unwrap();

        // bob sees X exactly once with protocol ts
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].virtual_id, 100);
        assert_eq!(list[0].title, "News 24");
        assert_eq!(list[0].logo, "http://logo.png");
        assert_eq!(list[0].category, "News");
        assert_eq!(list[0].protocol, "ts");
        assert!(!list[0].requires_free_slot);
    }

    #[tokio::test]
    async fn test_joinable_streams_predicates_and_filtering() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // 1. Feature disabled (Carol has joinable_streams: false) -> 403 feature_disabled
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.carol_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"feature_disabled");

        // Seed channels for Alice (filter: Group = "News")
        seed_test_m3u_items(
            &env.app_state,
            &[
                (201, "Sports", "Sports 1", "logo1"), // Filter hides
                (202, "News", "News 1", "logo2"),     // Other target
                (203, "News", "News 2", "logo3"),     // Non-shared
                (204, "News", "News 3", "logo4"),     // Own stream
                (205, "News", "News 4", "logo5"),     // Recording proxy user
                (206, "News", "News 5", "logo6"),     // Allowed joinable stream
            ],
        )
        .await;

        // 201: caller's filter hides X (Sports != News) -> not listed
        register_stream_ext(&env.app_state, 1, "dave", 1, 201, None, Some(101), PlaylistItemType::Live, true, None)
            .await;

        // 202: other target (target_id 99 != 1) -> not listed
        register_stream_ext(&env.app_state, 2, "dave", 99, 202, None, Some(102), PlaylistItemType::Live, true, None)
            .await;

        // 203: non-shared stream (shared = false) -> not listed
        register_stream_ext(&env.app_state, 3, "dave", 1, 203, None, None, PlaylistItemType::Live, false, None).await;

        // 204: own stream (username = alice) -> joinable from second device
        register_stream_ext(&env.app_state, 4, "alice", 1, 204, None, Some(104), PlaylistItemType::Live, true, None)
            .await;

        // 205: recording proxy user stream -> joinable on sharing target
        register_stream_ext(
            &env.app_state,
            5,
            tuliprox_core::model::RECORDING_PROXY_USERNAME,
            1,
            205,
            None,
            Some(105),
            PlaylistItemType::Live,
            true,
            None,
        )
        .await;

        // 206: valid shared stream on same target matching filter
        register_stream_ext(&env.app_state, 6, "dave", 1, 206, None, Some(106), PlaylistItemType::Live, true, None)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let list: Vec<JoinableStreamDto> = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.len(), 3);
        let vids: Vec<u32> = list.iter().map(|s| s.virtual_id).collect();
        assert!(vids.contains(&204), "own stream must be joinable from second device");
        assert!(vids.contains(&205), "recording proxy stream must be joinable");
        assert!(vids.contains(&206), "other user's stream must be joinable");
    }

    #[tokio::test]
    async fn test_joinable_streams_requires_free_slot_endpoint() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(&env.app_state, &[(300, "News", "Channel 300", "logo")]).await;

        // dave plays shared stream 300
        register_stream_ext(&env.app_state, 1, "dave", 1, 300, None, Some(300), PlaylistItemType::Live, true, None)
            .await;

        // Bob has max_connections: 2. Currently 0 active connections -> requires_free_slot is false
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let list: Vec<JoinableStreamDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].requires_free_slot);

        // Bob opens 1 stream (active_connections = 1 < 2) -> requires_free_slot is false
        register_stream_ext(&env.app_state, 2, "bob", 1, 301, None, None, PlaylistItemType::Live, false, None).await;
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        let list: Vec<JoinableStreamDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].requires_free_slot);

        // Bob opens 2nd stream (active_connections = 2 == max_connections 2) -> requires_free_slot is true
        register_stream_ext(&env.app_state, 3, "bob", 1, 302, None, None, PlaylistItemType::Live, false, None).await;
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        let list: Vec<JoinableStreamDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].requires_free_slot);
    }

    #[tokio::test]
    async fn test_joinable_streams_json_has_no_user_internal_fields() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(&env.app_state, &[(400, "News", "Channel 400", "logo400")]).await;
        register_stream_ext(&env.app_state, 1, "dave", 1, 400, None, Some(400), PlaylistItemType::Live, true, None)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let json_str = std::str::from_utf8(&body).unwrap();

        // Must not contain any sensitive internal fields
        assert!(!json_str.contains("username"));
        assert!(!json_str.contains("dave"));
        assert!(!json_str.contains("uid"));
        assert!(!json_str.contains("addr"));
        assert!(!json_str.contains("ip"));
        assert!(!json_str.contains("subscribers"));
        assert!(!json_str.contains("viewer"));

        // Must contain standard DTO fields
        assert!(json_str.contains("virtual_id"));
        assert!(json_str.contains("title"));
        assert!(json_str.contains("logo"));
        assert!(json_str.contains("category"));
        assert!(json_str.contains("protocol"));
        assert!(json_str.contains("requires_free_slot"));
    }

    #[tokio::test]
    async fn test_joinable_streams_hls_protocol() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(&env.app_state, &[(500, "News", "HLS Channel", "logo_hls")]).await;

        register_stream_ext(&env.app_state, 1, "dave", 1, 500, None, Some(500), PlaylistItemType::LiveHls, true, None)
            .await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let list: Vec<JoinableStreamDto> = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].virtual_id, 500);
        assert_eq!(list[0].protocol, "hls");
    }

    async fn get_joinable(router: axum::Router, token: &str) -> (StatusCode, Vec<JoinableStreamDto>, String) {
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let list = serde_json::from_str(&text).unwrap();
        (status, list, text)
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_lists_caller_copy() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 700,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/700.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 700)]);

        // Two running streams of the same channel in another target collapse into one entry.
        register_stream_channel(&env.app_state, 11, "dave", &fixture.foreign_channel(99, 900), None).await;
        register_stream_channel(&env.app_state, 12, "charlie", &fixture.foreign_channel(99, 901), None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].virtual_id, 700);
        assert_eq!(list[0].title, "News Caller");
        assert_eq!(list[0].logo, "logo-caller");
        assert_eq!(list[0].category, "News");
        assert_eq!(list[0].protocol, "ts");
        assert!(!list[0].requires_free_slot);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hidden_without_caller_channel() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 701,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/701.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, &[]).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 701)]);
        register_stream_channel(&env.app_state, 13, "dave", &fixture.foreign_channel(99, 900), None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list, []);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hidden_without_target_uuid_index() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 702,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/702.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        register_stream_channel(&env.app_state, 14, "dave", &fixture.foreign_channel(99, 900), None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list, []);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hidden_by_caller_filter() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Alice only allows Group = "News"; the caller channel sits in Sports.
        let fixture = CallerChannelFixture {
            vid: 703,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/703.ts",
            group: "Sports",
            title: "Sports Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 703)]);
        register_stream_channel(&env.app_state, 15, "dave", &fixture.foreign_channel(99, 900), None).await;

        let (status, list, _) = get_joinable(router.clone(), &env.alice_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list, []);

        // Bob has no filter and sees the same stream.
        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].virtual_id, 703);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hidden_when_provider_url_differs() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 704,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/704.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 704)]);
        let mut channel = fixture.foreign_channel(99, 900);
        channel.url = "http://provider.example/live/704-hd.ts".into();
        register_stream_channel(&env.app_state, 16, "dave", &channel, None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list, []);
    }

    #[tokio::test]
    async fn test_joinable_streams_dedupe_skips_invalid_candidate_and_keeps_valid_candidate() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 704,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/704.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 704)]);

        // First candidate: rejected because URL differs
        let mut channel_invalid = fixture.foreign_channel(99, 900);
        channel_invalid.url = "http://provider.example/live/704-hd.ts".into();
        register_stream_channel(&env.app_state, 16, "dave", &channel_invalid, None).await;

        // Second candidate: valid matching URL
        let mut channel_valid = fixture.foreign_channel(98, 901);
        channel_valid.url = "http://provider.example/live/704.ts".into();
        register_stream_channel(&env.app_state, 17, "carol", &channel_valid, None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.len(), 1, "valid candidate must be included even when first candidate is rejected");
        assert_eq!(list[0].virtual_id, 704);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hides_foreign_group_stream() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 705,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/705.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 705)]);
        let mut channel = fixture.foreign_channel(99, 900);
        channel.group_virtual_id = Some(950);
        register_stream_channel(&env.app_state, 17, "dave", &channel, None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list, []);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_json_has_no_foreign_fields() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 706,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/706.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 706)]);
        register_stream_channel(&env.app_state, 18, "dave", &fixture.foreign_channel(99, 906), None).await;

        let (status, list, text) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.len(), 1);

        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entry = &value[0];
        let mut keys: Vec<&str> = entry.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["category", "logo", "protocol", "requires_free_slot", "title", "virtual_id"]);
        assert_eq!(entry["virtual_id"], 706);
        assert!(!text.contains("906"), "foreign virtual id must not be exposed");
        assert!(!text.contains("Foreign Title"), "foreign title must not be exposed");
        assert!(!text.contains("foreign-group"));
        assert!(!text.contains("input_cross"), "provider input name must not be exposed");
        assert!(!text.contains("target_id"), "the foreign target must not be exposed");
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_requires_free_slot() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 707,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/707.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 707)]);
        register_stream_channel(&env.app_state, 19, "dave", &fixture.foreign_channel(99, 900), None).await;

        // Bob has max_connections 2; one active connection still leaves a free slot.
        register_stream_ext(&env.app_state, 20, "bob", 1, 801, None, None, PlaylistItemType::Live, false, None).await;
        let (_, list, _) = get_joinable(router.clone(), &env.bob_token).await;
        assert_eq!(list.len(), 1);
        assert!(!list[0].requires_free_slot);

        register_stream_ext(&env.app_state, 21, "bob", 1, 802, None, None, PlaylistItemType::Live, false, None).await;
        let (_, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(list.len(), 1);
        assert!(list[0].requires_free_slot);
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hls_uses_persisted_live_identity() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let fixture = CallerChannelFixture {
            vid: 709,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/709.m3u8",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 709)]);
        let mut channel = fixture.foreign_channel(99, 900);
        channel.item_type = PlaylistItemType::LiveHls;
        register_stream_channel(&env.app_state, 23, "dave", &channel, None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].virtual_id, 709);
        assert_eq!(list[0].protocol, "hls");
    }

    #[tokio::test]
    async fn test_joinable_streams_cross_target_hidden_when_caller_target_does_not_share() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));
        disable_target_sharing(&env.app_state);

        let fixture = CallerChannelFixture {
            vid: 708,
            input_name: "input_cross",
            provider_id: 7,
            url: "http://provider.example/live/708.ts",
            group: "News",
            title: "News Caller",
            logo: "logo-caller",
        };
        seed_caller_channels(&env.app_state, std::slice::from_ref(&fixture)).await;
        seed_target_uuid_index(&env.app_state, "main", &[(fixture.uuid(), 708)]);
        register_stream_channel(&env.app_state, 22, "dave", &fixture.foreign_channel(99, 900), None).await;

        let (status, list, _) = get_joinable(router, &env.bob_token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list, []);
    }

    #[tokio::test]
    async fn test_get_me_channels_content_filter_and_query() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(
            &env.app_state,
            &[
                (601, "News", "CNN International", "logo1"),
                (602, "News", "BBC News", "logo2"),
                (603, "Sports", "ESPN Sports", "logo3"),
            ],
        )
        .await;

        // Alice has filter Group = "News" -> Sports 603 must be excluded
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let channels: Vec<shared::model::UserChannelDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(channels.len(), 2);
        assert!(channels.iter().any(|c| c.virtual_id == 601 && c.name == "CNN International"));
        assert!(channels.iter().any(|c| c.virtual_id == 602 && c.name == "BBC News"));
        assert!(!channels.iter().any(|c| c.virtual_id == 603));

        // Query by name "BBC"
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels?query=BBC")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let channels: Vec<shared::model::UserChannelDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].virtual_id, 602);
        assert_eq!(channels[0].name, "BBC News");

        // Bob has no filter -> sees all 3 channels
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let channels: Vec<shared::model::UserChannelDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(channels.len(), 3);
    }

    #[tokio::test]
    async fn test_get_me_epg_filter_and_window_validation() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(
            &env.app_state,
            &[(701, "News", "News Channel", "logo1"), (702, "Sports", "Sports Channel", "logo2")],
        )
        .await;

        // Alice cannot access channel 702 (Sports) because it is filtered -> 404
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/epg?vid=702")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Window > 48 hours -> 400
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/epg?vid=701&from=1700000000&to=1700200000")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // to <= from -> 400
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/epg?vid=701&from=1700001000&to=1700000000")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_epg_timeshift_exact_iso_formatting() {
        let ts = 1_700_000_000_i64; // 2023-11-14T22:13:20Z
        let shift_plus_2 = tuliprox_core::utils::epg_parser::parse_timeshift(Some("+02:00"));
        let formatted = tuliprox_core::utils::epg_parser::format_iso_time_with_timeshift(ts, &shift_plus_2);
        assert_eq!(formatted, "2023-11-15T00:13:20+02:00");

        let shift_minus_5 = tuliprox_core::utils::epg_parser::parse_timeshift(Some("-05:00"));
        let formatted_neg = tuliprox_core::utils::epg_parser::format_iso_time_with_timeshift(ts, &shift_minus_5);
        assert_eq!(formatted_neg, "2023-11-14T17:13:20-05:00");
    }

    #[tokio::test]
    async fn test_admin_kick_stream_blocks_for_kick_secs_and_hls_termination() {
        let env = setup_test_env();

        // 1. Socket-bound stream: kicked with block_secs = 60
        let _stream_uid =
            register_stream(&env.app_state, 1, "alice", 101, None, PlaylistItemType::Live, false, None).await;
        let streams = env.app_state.active_users.user_streams("alice").await;
        let stream = streams.into_iter().find(|s| s.channel.virtual_id == 101).expect("stream found");
        let _close_rx = env.app_state.connection_manager.register_close_socket(stream.addr);
        let kicked = kick_stream(&env.app_state, &stream, 60, shared::model::StreamEndedReason::Kicked).await;
        assert!(kicked, "socket stream must be kicked");
        assert!(
            env.app_state
                .connection_manager
                .user_manager
                .is_user_blocked_for_stream("alice", VirtualId::new(101))
                .await,
            "user must be blocked for stream in active user manager"
        );

        // 2. HLS stream (no socket closer): kick_connection returns false, but terminate_session_token returns true
        let bob_user = env.app_state.app_config.get_user_credentials("bob").unwrap();
        let hls_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let token = env
            .app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &bob_user,
                session_token: "sess-bob-hls",
                virtual_id: 102,
                provider: "prov",
                stream_url: "http://example.com/102.m3u8",
                addr: &hls_addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let _hls_uid =
            register_stream(&env.app_state, 2, "bob", 102, None, PlaylistItemType::LiveHls, false, Some(token.clone()))
                .await;
        let bob_streams = env.app_state.active_users.user_streams("bob").await;
        let hls_stream = bob_streams.into_iter().find(|s| s.channel.virtual_id == 102).expect("hls stream found");
        assert!(env.app_state.active_users.get_and_update_user_session("bob", &token).await.is_some());

        let hls_kicked = kick_stream(&env.app_state, &hls_stream, 60, shared::model::StreamEndedReason::Kicked).await;
        assert!(hls_kicked, "HLS session termination must return true even if socket is not kicked");
        assert!(env.app_state.active_users.get_and_update_user_session("bob", &token).await.is_none());
    }

    #[tokio::test]
    async fn test_hls_cache_member_reported_in_streams_and_failover_members() {
        let env = setup_test_env();
        let group = seed_group(&env.app_state, 1000, &[1001, 1002]);
        seed_m3u_group_item(&env.app_state, &group, "News", "Group Channel News").await;

        let group_key = tuliprox_hls::api::HlsSessionKey::for_group(1, 1000);
        let hls_session = env.app_state.hls.proxy.sessions().get_or_create_session(group_key, b"secret", 0).await;
        hls_session.write().await.origin_source = tuliprox_hls::api::HlsOriginSource::new(
            1,
            Arc::from("input_1"),
            "stream_1",
            tuliprox_hls::api::HlsOriginSourceKind::M3uMediaPlaylist,
        );

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut stream = StreamInfo::new(shared::model::StreamInfoParams {
            uid: 10,
            meter_uid: 0,
            username: "alice",
            addr: &addr,
            client_ip: "127.0.0.1",
            provider: Arc::from("test_prov"),
            stream_channel: StreamChannel {
                target_id: 1,
                virtual_id: 1000,
                group_virtual_id: Some(1000),
                item_type: PlaylistItemType::LiveHls,
                shared: true,
                ..StreamChannel::default()
            },
            user_agent: "test".to_string(),
            country_code: None,
            session_token: Some("dummy-session-token"),
        });
        stream.member_info = None; // In HLS cache, member_info is None!

        let resolved_vid = resolve_stream_member_vid(&env.app_state, &stream, Some(&group)).await;
        assert_eq!(resolved_vid, Some(1002), "member_vid must resolve to 1002 from session origin_source");
    }

    #[tokio::test]
    async fn test_channels_live_item_type_restriction() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(&env.app_state, &[(701, "News", "Channel Live", "logo1")]).await;

        // Alice allows Live -> 1 channel
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let channels: Vec<shared::model::UserChannelDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(channels.len(), 1);

        // Modify Alice's allowed clusters to Vod only (no Live)
        let mut api_proxy = (*env.app_state.app_config.api_proxy.load_full().unwrap()).clone();
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                if cred.username == "alice" {
                    Arc::make_mut(cred).output_clusters = ClusterFlags::Vod;
                }
            }
        }
        env.app_state.app_config.set_api_proxy(api_proxy).unwrap();

        // Alice disallowed Live -> 0 channels returned
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/channels")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let channels: Vec<shared::model::UserChannelDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(channels.len(), 0, "disallowed cluster/item-type must yield 0 channels");
    }

    #[tokio::test]
    async fn test_get_me_streams_joinable_with_metrics_disabled_lists_both() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        seed_test_m3u_items(
            &env.app_state,
            &[(501, "News", "Channel 501", "logo501"), (502, "Sports", "Channel 502", "logo502")],
        )
        .await;

        // Two shared streams, both with shared_stream_id: Some(0) (metrics disabled default)
        register_stream_ext(&env.app_state, 1, "dave", 1, 501, None, Some(0), PlaylistItemType::Live, true, None).await;
        register_stream_ext(&env.app_state, 2, "dave", 1, 502, None, Some(0), PlaylistItemType::Live, true, None).await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams/joinable")
            .header("Authorization", format!("Bearer {}", env.bob_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let list: Vec<JoinableStreamDto> =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        // Both streams must be listed because 0 is treated as absent and keys fall back to (target_id, client_vid)
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn test_get_me_streams_no_session_write_on_get() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let token = "test-session-token-read-only";
        let user = env.app_state.app_config.get_user_credentials("alice").unwrap();
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let _ = env
            .app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: 101,
                provider: "test-prov",
                stream_url: "http://example.com/hls/101.m3u8",
                addr: &addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(tuliprox_session::active_provider_manager::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let _ = register_stream(
            &env.app_state,
            1,
            "alice",
            101,
            None,
            PlaylistItemType::LiveHls,
            false,
            Some(token.to_string()),
        )
        .await;

        let before = env.app_state.active_users.get_user_session("alice", token).await.expect("session exists");
        tokio::time::sleep(tokio::time::Duration::from_millis(15)).await;

        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/me/streams")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let after = env.app_state.active_users.get_user_session("alice", token).await.expect("session exists");
        // session.ts must NOT be updated by GET /me/streams
        assert_eq!(before.ts, after.ts);
    }

    #[tokio::test]
    async fn test_failover_switch_live_item_with_m3u8_url_dispatches_hls_switch() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let _group = seed_group(&env.app_state, 2000, &[2001, 2002]);
        let uid = register_stream_full(
            &env.app_state,
            1,
            "alice",
            1,
            2000,
            None,
            None,
            PlaylistItemType::Live,
            false,
            Some("session-tok".to_string()),
            Some("http://example.invalid/live/stream.m3u8"),
        )
        .await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2002}"#))
            .unwrap();
        let res = router.oneshot(req).await.unwrap();

        // Because it classifies as LiveHls, it dispatches to HLS switch which returns 502 "playback session not active"
        // whereas TS switch would return 404 "stream not registered"
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"playback session not active");
    }

    #[tokio::test]
    async fn test_me_stream_failover_switch_legacy_hls_does_not_leak_auth_or_cookies() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tuliprox_session::MemberBinding;

        let captured_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured_requests);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..10 {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 4096];
                let Ok(n) = socket.read(&mut buf).await else { break };
                if n == 0 {
                    break;
                }
                let req_text = String::from_utf8_lossy(&buf[..n]).to_string();
                captured_clone.lock().unwrap().push(req_text);

                let body = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg1.ts\n";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let group_vid = 4000;
        let member_vid_0 = 4001;
        let member_vid_1 = 4002;

        let members = vec![
            GroupMember {
                member_vid: VirtualId::new(member_vid_0),
                input_name: "input_0".into(),
                provider_id: 1,
                input_stream_id: "stream_0".into(),
                url: format!("http://example.invalid/{member_vid_0}.m3u8").into(),
                item_type: PlaylistItemType::LiveHls,
                cluster: XtreamCluster::Live,
                title: "Member 0".into(),
                archive: None,
                epg_channel_id: None,
                additional_properties: None,
                input_url: Arc::from(""),
                upstream_user_agent: None,
            },
            GroupMember {
                member_vid: VirtualId::new(member_vid_1),
                input_name: "input_1".into(),
                provider_id: 2,
                input_stream_id: "stream_1".into(),
                url: format!("http://{mock_addr}/live.m3u8").into(),
                item_type: PlaylistItemType::LiveHls,
                cluster: XtreamCluster::Live,
                title: "Member 1".into(),
                archive: None,
                epg_channel_id: None,
                additional_properties: None,
                input_url: Arc::from(""),
                upstream_user_agent: None,
            },
        ];

        let record = FailoverGroupRecord {
            group_vid: VirtualId::new(group_vid),
            key: "test_hls_leak_group".into(),
            origin: FailoverGroupOrigin::Auto,
            policy: FailoverGroupPolicy::Auto,
            members,
            orphaned: Vec::new(),
        };

        let input0 = Arc::new(ConfigInput {
            id: 1,
            name: "input_0".into(),
            input_type: InputType::M3u,
            url: "http://127.0.0.1:9/m3u".into(),
            priority: 1,
            max_connections: 10,
            enabled: true,
            provider_configs: Some(vec![Arc::new(ConfigProvider {
                name: "input_0".into(),
                urls: vec!["http://127.0.0.1:9/m3u".into()],
                provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
                dns: None,
                current_url_index: std::sync::atomic::AtomicUsize::new(0),
                dns_cache: Arc::default(),
            })]),
            ..ConfigInput::default()
        });
        let input1 = Arc::new(ConfigInput {
            id: 2,
            name: "input_1".into(),
            input_type: InputType::M3u,
            url: format!("http://{mock_addr}/m3u"),
            priority: 1,
            max_connections: 10,
            enabled: true,
            provider_configs: Some(vec![Arc::new(ConfigProvider {
                name: "input_1".into(),
                urls: vec![format!("http://{mock_addr}/m3u").into()],
                provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
                dns: None,
                current_url_index: std::sync::atomic::AtomicUsize::new(0),
                dns_cache: Arc::default(),
            })]),
            ..ConfigInput::default()
        });
        let target = env.app_state.app_config.get_target_by_id(1).unwrap();
        env.app_state.app_config.sources.store(Arc::new(SourcesConfig {
            inputs: vec![Arc::clone(&input0), Arc::clone(&input1)],
            sources: vec![ConfigSource {
                inputs: vec![Arc::clone(&input0.name), Arc::clone(&input1.name)],
                targets: vec![target],
            }],
            ..Default::default()
        }));
        env.app_state.active_provider.update_config(&env.app_state.app_config);

        let storage = get_target_storage_path(&env.app_state.app_config.config.load(), "main").unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        FailoverGroupStore::write(&storage, std::slice::from_ref(&record), &[], None).unwrap();
        seed_m3u_group_item(&env.app_state, &record, "News", "Group Channel News").await;
        env.app_state.playlists.bump_failover_group_generation("main");

        let session_token = "sess-alice-legacy-hls";
        let uid = register_stream(
            &env.app_state,
            1,
            "alice",
            group_vid,
            None,
            PlaylistItemType::LiveHls,
            false,
            Some(session_token.to_string()),
        )
        .await;

        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let alice_user = env.app_state.app_config.get_user_credentials("alice").unwrap();
        env.app_state
            .active_users
            .create_user_session(CreateUserSessionParams {
                user: &alice_user,
                session_token,
                virtual_id: group_vid,
                provider: &record.members[0].input_name,
                stream_url: &record.members[0].url,
                addr: &addr,
                connection_permission: UserConnectionPermission::Allowed,
                connection_kind: Some(ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;

        let binding = MemberBinding {
            group_vid,
            member_vid: member_vid_0,
            member_index: 0,
            input_name: Arc::clone(&record.members[0].input_name),
            member_title: "Member 0".into(),
            provider: Arc::clone(&record.members[0].input_name),
        };
        env.app_state.active_users.bind_member("alice", session_token, binding).await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Cookie", "session=supersecret_cookie_leak_test")
            .header("Content-Type", "application/json")
            .body(Body::from(format!(r#"{{"member_vid":{member_vid_1}}}"#)))
            .unwrap();

        let res = router.oneshot(req).await.unwrap();
        let status = res.status();
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::OK, "legacy switch response: {status}: {body_str}");

        let captured = captured_requests.lock().unwrap();
        assert!(!captured.is_empty(), "upstream mock must receive probe request");
        for req in captured.iter() {
            assert!(
                !req.to_ascii_lowercase().contains("authorization:"),
                "upstream must not receive Authorization header, got: {req}"
            );
            assert!(
                !req.to_ascii_lowercase().contains("cookie:"),
                "upstream must not receive Cookie header, got: {req}"
            );
            assert!(
                req.to_ascii_lowercase().contains("user-agent:"),
                "upstream must receive User-Agent header, got: {req}"
            );
        }
    }

    #[tokio::test]
    async fn test_split_stream_gate_forbidden_403() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Carol has source_switch: false
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/me/streams/100/split")
            .header("Authorization", format!("Bearer {}", env.carol_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"feature_disabled");
    }

    #[tokio::test]
    async fn test_split_stream_foreign_uid_and_nonexistent_404() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Register stream belonging to Bob
        let uid_bob = register_stream(&env.app_state, 1, "bob", 2000, None, PlaylistItemType::Live, true, None).await;

        // Alice tries to split Bob's stream -> 404
        let req_foreign = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_bob}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_foreign = router.clone().oneshot(req_foreign).await.unwrap();
        assert_eq!(res_foreign.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(res_foreign.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"stream not found");

        // Alice tries to split nonexistent stream -> 404
        let req_nonexistent = Request::builder()
            .method("POST")
            .uri("/api/v1/me/streams/99999/split")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_nonexistent = router.oneshot(req_nonexistent).await.unwrap();
        assert_eq!(res_nonexistent.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(res_nonexistent.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"stream not found");
    }

    #[tokio::test]
    async fn test_split_stream_hls_unsupported_409() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let uid_hls = register_stream(
            &env.app_state,
            1,
            "alice",
            3000,
            None,
            PlaylistItemType::LiveHls,
            true,
            Some("sess-alice-hls".to_string()),
        )
        .await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_hls}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"split_unsupported_protocol");
    }

    #[tokio::test]
    async fn test_split_stream_not_shared_409() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Stream with shared: false
        let uid = register_stream(&env.app_state, 1, "alice", 2000, None, PlaylistItemType::Live, false, None).await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"stream_not_shared");
    }

    #[tokio::test]
    async fn test_split_stream_single_subscriber_409() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        let uid = register_stream(&env.app_state, 1, "alice", 2000, None, PlaylistItemType::Live, true, None).await;

        // Set subscriber count to 1
        let key = crate::api::api_utils::failover_group::group_shared_key(1, 2000);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key, 1).await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"stream_not_shared");
    }

    #[tokio::test]
    async fn test_split_stream_success_and_cooldown_429() {
        use shared::model::SplitStreamResponseDto;

        tokio::time::pause();

        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));
        let _group = seed_group(&env.app_state, 2000, &[2001, 2002]);

        let uid = register_stream(&env.app_state, 1, "alice", 2000, None, PlaylistItemType::Live, true, None).await;

        // Shared stream with 2 subscribers
        let key = crate::api::api_utils::failover_group::group_shared_key(1, 2000);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key, 2).await;

        // First split succeeds -> 200 OK
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let dto: SplitStreamResponseDto = serde_json::from_slice(&body).unwrap();
        assert!(dto.reconnect_required);
        assert_eq!(dto.url, "/live/alice/pass/2000.ts?dedicated=1");

        // Immediate second split within 2s -> 429 switch_cooldown with exact Retry-After: 2
        let req_split2 = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_split2 = router.clone().oneshot(req_split2).await.unwrap();
        assert_eq!(res_split2.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res_split2.headers().get("retry-after").unwrap(), "2");
        let body_split2 = to_bytes(res_split2.into_body(), 4096).await.unwrap();
        assert_eq!(&body_split2[..], b"switch_cooldown");

        // Failover switch on same stream within 2s cooldown -> 429 switch_cooldown
        let req_switch = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/failover-switch"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"member_vid":2002}"#))
            .unwrap();
        let res_switch = router.clone().oneshot(req_switch).await.unwrap();
        assert_eq!(res_switch.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res_switch.headers().get("retry-after").unwrap(), "2");
        let body_switch = to_bytes(res_switch.into_body(), 4096).await.unwrap();
        assert_eq!(&body_switch[..], b"switch_cooldown");

        // Advance tokio time past cooldown (2.1s)
        tokio::time::advance(Duration::from_millis(2100)).await;

        // Split succeeds again after cooldown expires
        let req_split3 = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_split3 = router.oneshot(req_split3).await.unwrap();
        assert_eq!(res_split3.status(), StatusCode::OK);
        let body_split3 = to_bytes(res_split3.into_body(), 4096).await.unwrap();
        let dto3: SplitStreamResponseDto = serde_json::from_slice(&body_split3).unwrap();
        assert!(dto3.reconnect_required);
        assert_eq!(dto3.url, "/live/alice/pass/2000.ts?dedicated=1");
    }

    #[tokio::test]
    async fn test_split_stream_m3u_only_target_returns_m3u_stream_path() {
        use shared::model::SplitStreamResponseDto;
        use tuliprox_core::model::{M3uTargetOutput, TargetOutput};

        let env = setup_test_env();
        let target = env.app_state.app_config.get_target_by_id(1).unwrap();
        let mut m3u_target = (*target).clone();
        m3u_target.output =
            vec![TargetOutput::M3u(M3uTargetOutput::from(&shared::model::M3uTargetOutputDto::default()))];
        env.app_state.app_config.sources.store(Arc::new(SourcesConfig {
            inputs: Vec::new(),
            sources: vec![ConfigSource { inputs: Vec::new(), targets: vec![Arc::new(m3u_target)] }],
            ..Default::default()
        }));

        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));
        let _group = seed_group(&env.app_state, 2000, &[2001, 2002]);

        let uid = register_stream(&env.app_state, 1, "alice", 2000, None, PlaylistItemType::Live, true, None).await;

        let key = crate::api::api_utils::failover_group::group_shared_key(1, 2000);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key, 2).await;

        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res = router.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = to_bytes(res.into_body(), 4096).await.unwrap();
        let dto: SplitStreamResponseDto = serde_json::from_slice(&body).unwrap();
        assert!(dto.reconnect_required);
        let expected_url =
            format!("/{}/live/alice/pass/2000.ts?dedicated=1", crate::repository::storage_const::M3U_STREAM_PATH);
        assert_eq!(dto.url, expected_url);
    }

    #[tokio::test]
    async fn test_split_stream_catchup_and_vod_rejected_409() {
        let env = setup_test_env();
        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Catchup stream
        let uid_catchup =
            register_stream(&env.app_state, 1, "alice", 2010, None, PlaylistItemType::Catchup, true, None).await;
        let key_catchup = crate::api::api_utils::failover_group::group_shared_key(1, 2010);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key_catchup, 2).await;

        let req_catchup = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_catchup}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_catchup = router.clone().oneshot(req_catchup).await.unwrap();
        assert_eq!(res_catchup.status(), StatusCode::CONFLICT);
        let body_catchup = to_bytes(res_catchup.into_body(), 4096).await.unwrap();
        assert_eq!(&body_catchup[..], b"split_unsupported_protocol");

        // VOD stream
        let uid_vod =
            register_stream(&env.app_state, 1, "alice", 2020, None, PlaylistItemType::Video, true, None).await;
        let key_vod = crate::api::api_utils::failover_group::group_shared_key(1, 2020);
        env.app_state.shared_stream_manager.set_test_subscriber_count(&key_vod, 2).await;

        let req_vod = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/me/streams/{uid_vod}/split"))
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        let res_vod = router.oneshot(req_vod).await.unwrap();
        assert_eq!(res_vod.status(), StatusCode::CONFLICT);
        let body_vod = to_bytes(res_vod.into_body(), 4096).await.unwrap();
        assert_eq!(&body_vod[..], b"split_unsupported_protocol");
    }

    #[test]
    fn test_is_dedicated_requested_parser() {
        use crate::api::api_utils::is_dedicated_requested;

        assert!(!is_dedicated_requested(None));
        assert!(!is_dedicated_requested(Some("")));
        assert!(!is_dedicated_requested(Some("foo=bar")));
        assert!(!is_dedicated_requested(Some("dedicated=0")));
        assert!(!is_dedicated_requested(Some("dedicated=true")));
        assert!(!is_dedicated_requested(Some("dedicated=10")));
        assert!(is_dedicated_requested(Some("dedicated=1")));
        assert!(is_dedicated_requested(Some("foo=bar&dedicated=1")));
        assert!(is_dedicated_requested(Some("dedicated=1&foo=bar")));
        assert!(is_dedicated_requested(Some("a=1&dedicated=1&b=2")));
    }

    #[tokio::test]
    async fn test_playback_dedicated_flag_decision() {
        use crate::api::api_utils::{stream_response, PlaybackSource};
        use bytes::Bytes;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tuliprox_core::model::SharedSubscriberId;
        use tuliprox_session::streams::shared_stream_manager::{SharedStreamCtx, SharedStreamManager};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..10 {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                let mut buf = [0u8; 4096];
                let Ok(n) = socket.read(&mut buf).await else { break };
                if n == 0 {
                    break;
                }
                let body = "mock-ts-stream-content";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let env = setup_test_env();

        let stream_url_string = format!("http://{mock_addr}/live/test_dedicated.ts");
        let stream_url = stream_url_string.as_str();

        let input0 = Arc::new(ConfigInput {
            id: 1,
            name: "input_0".into(),
            input_type: InputType::M3u,
            url: format!("http://{mock_addr}/m3u"),
            priority: 1,
            max_connections: 10,
            enabled: true,
            provider_configs: Some(vec![Arc::new(ConfigProvider {
                name: "input_0".into(),
                urls: vec![format!("http://{mock_addr}/m3u").into()],
                provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::default(),
                dns: None,
                current_url_index: std::sync::atomic::AtomicUsize::new(0),
                dns_cache: Arc::default(),
            })]),
            ..ConfigInput::default()
        });
        let target = env.app_state.app_config.get_target_by_id(1).unwrap();
        env.app_state.app_config.sources.store(Arc::new(SourcesConfig {
            inputs: vec![Arc::clone(&input0)],
            sources: vec![ConfigSource { inputs: vec![Arc::clone(&input0.name)], targets: vec![Arc::clone(&target)] }],
            ..Default::default()
        }));
        env.app_state.active_provider.update_config(&env.app_state.app_config);

        // Register initial shared stream
        let owner_addr: SocketAddr = "127.0.0.1:55140".parse().unwrap();
        let owner_handle = env
            .app_state
            .active_provider
            .acquire_connection(&input0.name, &owner_addr, 0, ConnectionKind::Normal)
            .expect("owner allocation");
        let subscriber_id = SharedSubscriberId::from_stream_uid(env.app_state.connection_manager.next_stream_uid());
        let pending_cleanup = SharedStreamManager::reserve_subscriber_cleanup(
            &env.app_state.connection_manager,
            subscriber_id,
            owner_addr,
        )
        .await
        .expect("reserve subscriber cleanup");
        let shared_stream = futures::stream::pending::<Result<Bytes, std::io::Error>>();
        let _registered = SharedStreamManager::register_shared_stream(
            SharedStreamCtx {
                app_config: &env.app_state.app_config,
                shared_stream_manager: &env.app_state.shared_stream_manager,
                active_provider: &env.app_state.active_provider,
                connection_manager: &env.app_state.connection_manager,
            },
            stream_url,
            shared_stream,
            &owner_addr,
            subscriber_id,
            Vec::new(),
            1,
            Some(tuliprox_session::ManagedProviderHandle::new(
                Arc::clone(&env.app_state.active_provider),
                owner_handle,
            )),
            pending_cleanup,
            0,
            ConnectionKind::Normal,
            "owner_user",
            "owner_token",
        )
        .await;

        assert_eq!(env.app_state.shared_stream_manager.subscriber_count(stream_url).await, 1);
        assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);

        let alice_user = env.app_state.app_config.get_user_credentials("alice").unwrap();
        assert!(alice_user.player_features.source_switch);
        let carol_user = env.app_state.app_config.get_user_credentials("carol").unwrap();
        assert!(!carol_user.player_features.source_switch);

        let make_channel = || StreamChannel {
            target_id: 1,
            virtual_id: 2000,
            group_virtual_id: None,
            url: stream_url.into(),
            item_type: PlaylistItemType::Live,
            shared: true,
            ..StreamChannel::default()
        };

        // 1. dedicated=false with alice -> joins shared stream (subscriber count becomes 2)
        let addr1: SocketAddr = "127.0.0.1:55151".parse().unwrap();
        let fp1 = Fingerprint::new(format!("fp-{addr1}"), addr1.ip().to_string(), addr1);
        let outcome1 = stream_response(
            &fp1,
            &env.app_state,
            "sess-1",
            None,
            make_channel(),
            PlaybackSource::Single { input: &input0, stream_url },
            None,
            &HeaderMap::default(),
            &target,
            &alice_user,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
            None,
            false,
            false,
        )
        .await;
        assert!(outcome1.is_success());
        assert_eq!(env.app_state.shared_stream_manager.subscriber_count(stream_url).await, 2);
        assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);

        // 2. dedicated=true with carol (source_switch: false) -> still joins shared stream (subscriber count becomes 3)
        let addr2: SocketAddr = "127.0.0.1:55152".parse().unwrap();
        let fp2 = Fingerprint::new(format!("fp-{addr2}"), addr2.ip().to_string(), addr2);
        let outcome2 = stream_response(
            &fp2,
            &env.app_state,
            "sess-2",
            None,
            make_channel(),
            PlaybackSource::Single { input: &input0, stream_url },
            None,
            &HeaderMap::default(),
            &target,
            &carol_user,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
            None,
            false,
            true, // dedicated requested, but Carol's source_switch is false!
        )
        .await;
        assert!(outcome2.is_success());
        assert_eq!(env.app_state.shared_stream_manager.subscriber_count(stream_url).await, 3);
        assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 1);

        // 3. dedicated=true with alice (source_switch: true) -> skips shared join, acquires own provider slot
        let addr3: SocketAddr = "127.0.0.1:55153".parse().unwrap();
        let fp3 = Fingerprint::new(format!("fp-{addr3}"), addr3.ip().to_string(), addr3);
        let outcome3 = stream_response(
            &fp3,
            &env.app_state,
            "sess-3",
            None,
            make_channel(),
            PlaybackSource::Single { input: &input0, stream_url },
            None,
            &HeaderMap::default(),
            &target,
            &alice_user,
            UserConnectionPermission::Allowed,
            ConnectionKind::Normal,
            false,
            None,
            false,
            true, // dedicated requested and Alice's source_switch is true!
        )
        .await;
        assert!(
            outcome3.is_success(),
            "outcome3 failed: status={:?}, class={:?}",
            outcome3.response.status(),
            outcome3.failure_class
        );
        // Shared stream subscribers remain 3 (Alice did NOT join the shared stream)
        assert_eq!(env.app_state.shared_stream_manager.subscriber_count(stream_url).await, 3);
        // Instead, a second provider connection was acquired for the dedicated stream!
        assert_eq!(env.app_state.active_provider.get_provider_connections_count(), 2);
    }

    #[tokio::test]
    async fn test_me_capabilities_enforces_network_access() {
        use shared::model::NetworkAccessDto;

        let env = setup_test_env();
        // Update alice to have network_access restricting to 10.0.0.0/8
        {
            let mut api_proxy = (*env.app_state.app_config.api_proxy.load_full().unwrap()).clone();
            Arc::make_mut(&mut api_proxy.user[0].credentials[0]).network_access =
                Some(tuliprox_core::model::NetworkAccess::from(&NetworkAccessDto {
                    allowed_countries: None,
                    allowed_networks: Some(vec!["10.0.0.0/8".to_string()]),
                }));
            env.app_state.app_config.set_api_proxy(api_proxy).unwrap();
        }

        let router = me_player_api_register("").with_state(Arc::clone(&env.app_state));

        // Denied IP (192.168.1.1): 403 Forbidden
        let mut req_denied = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        req_denied.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([192, 168, 1, 1], 12345))));
        let res_denied = router.clone().oneshot(req_denied).await.unwrap();
        assert_eq!(res_denied.status(), StatusCode::FORBIDDEN);

        // Allowed IP (10.0.0.1): 200 OK
        let mut req_allowed = Request::builder()
            .method("GET")
            .uri("/api/v1/me/capabilities")
            .header("Authorization", format!("Bearer {}", env.alice_token))
            .body(Body::empty())
            .unwrap();
        req_allowed.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 12345))));
        let res_allowed = router.oneshot(req_allowed).await.unwrap();
        assert_eq!(res_allowed.status(), StatusCode::OK);
    }
}
