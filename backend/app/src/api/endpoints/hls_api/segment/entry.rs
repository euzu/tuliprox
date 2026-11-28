use super::{
    append_user_session_provider_headers, build_hls_manifest_request_headers, build_hls_origin_resolution,
    build_hls_origin_source_for_playback, create_hls_cache_entry_master_playlist_response_with_key_and_playback_id,
    create_hls_cache_entry_master_playlist_response_with_playback_id, download_legacy_hls_manifest,
    ensure_hls_manifest_extension, hls_cache_enabled_for_user, hls_custom_video_manifest_response,
    hls_entry_user_session_token, hls_media_playlist_wrap_enabled, hls_panel_provisioning_or_status_response,
    hls_playback_kind, hls_response, m3u_archive_epg_reference_ts, m3u_catchup_epg_reference_from_session_token,
    normalize_xtream_live_hls_url, release_prepared_hls_manifest_session, stalker_hls_stream, stalker_manifest_url,
    terminate_failed_hls_manifest_session, try_reserve_hls_entry_origin_account_for_redirect, wrap_media_playlist,
    HlsEntryStreamContext, HlsMediaPlaylistWrap, HlsOriginEntryUrl, HlsRequestStage, StalkerManifestUrl,
};
use crate::{
    api::{
        api_utils::{
            blocks_new_playback, connection_priority_for_kind, effective_playback_id_for_user,
            failover_group::record_member_failure, get_hls_playback_ttl_secs, group_playback_settings,
            is_running_playback, resolve_stalker_stream_for_account, select_provider_stream_url, stalker_stream_kind,
            GroupSelectionRequest,
        },
        model::{AppState, CustomVideoStreamType, PlaybackLeaseRef, UserSession},
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, InputSource, ProxyUserCredentials},
    processing::parser::hls::{
        classify_hls_playlist, media_playlist_discontinuity_sequence, media_playlist_media_sequence,
        media_playlist_segment_count, rewrite_hls, HlsManifestSource, HlsMediaSequenceRebase, HlsPlaylistKind,
        RewriteHlsProps,
    },
    repository::{m3u_get_item_for_stream_id, storage_const, xtream_get_item_for_stream_id},
    utils::{debug_if_enabled, request},
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::{error, warn};
use shared::{
    defaults::HLS_EXT,
    model::{PlaylistItemType, StreamChannel, TargetType, UserConnectionPermission, VirtualId},
    utils::sanitize_sensitive_info,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tuliprox_core::model::PlaybackKind;
use tuliprox_hls::api::{
    build_hls_origin_session_owner, build_proxy_session_id, extract_hls_provider_session_headers,
    HlsMemberUnhealthyReason, HlsOriginSource, HlsSessionKey,
};
use tuliprox_repository::{
    target_stream_item::{classify_virtual_id, ResolvedStreamTarget},
    FailoverGroupRecord,
};
use tuliprox_session::{
    failover_group::{BudgetDecision, FailoverBudget, FailureClass, HalfOpenGuard, SelectionMode},
    ConnectionManager, LegacyHlsRebase, ManagedProviderHandle, MemberBinding,
};

pub(in crate::api::endpoints::hls_api) struct HlsCacheManifestOrigin<'a> {
    pub(in crate::api::endpoints::hls_api) raw_request_url: &'a str,
    pub(in crate::api::endpoints::hls_api) session_entry_url: HlsOriginEntryUrl,
    pub(in crate::api::endpoints::hls_api) input: &'a ConfigInput,
    pub(in crate::api::endpoints::hls_api) origin_source: HlsOriginSource,
}

pub(in crate::api::endpoints::hls_api) struct HlsCacheOriginResolution {
    pub(in crate::api::endpoints::hls_api) hls_url: String,
    pub(in crate::api::endpoints::hls_api) session_entry_url: HlsOriginEntryUrl,
}

/// Proxy path of the HLS entry playlist for `virtual_id`: the M3U path for M3U-only targets, else the Xtream path.
pub(in crate::api) fn build_virtual_hls_entry_path(
    target: &ConfigTarget,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    virtual_id: u32,
) -> String {
    if input.input_type.is_m3u() && !target.has_output(TargetType::Xtream) {
        format!("/{}/live/{}/{}/{}{HLS_EXT}", storage_const::M3U_STREAM_PATH, user.username, user.password, virtual_id)
    } else {
        format!("/live/{}/{}/{}{HLS_EXT}", user.username, user.password, virtual_id)
    }
}

/// Serves an HLS manifest request for the selected provider account; a Stalker manifest URL is accepted, resolved or
/// rejected per [`stalker_manifest_url`].
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(in crate::api) async fn handle_hls_stream_request(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    user_session: Option<&UserSession>,
    session_token_hint: Option<&str>,
    hls_url: &str,
    archive_reference: Option<i64>,
    stream_context: HlsEntryStreamContext,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    connection_permission: UserConnectionPermission,
    connection_kind: Option<crate::api::model::ConnectionKind>,
    original_hls_entry_path: &str,
    stage: HlsRequestStage<'_>,
) -> impl IntoResponse + Send {
    let virtual_id = stream_context.virtual_id();
    if app_state.active_users.is_user_blocked_for_stream(&user.username, VirtualId::new(virtual_id)).await {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }

    let stream_ref = stream_context.stream_ref().to_string();
    // Variant URLs were normalized before sealing or come from an upstream master; use them as is.
    let url = if stage.normalizes_url() {
        let normalized_hls_url = normalize_xtream_live_hls_url(hls_url, input);
        if normalized_hls_url != hls_url {
            debug_if_enabled!(
                "Normalized xtream hls url from {} to {}",
                sanitize_sensitive_info(hls_url),
                sanitize_sensitive_info(&normalized_hls_url)
            );
        }
        ensure_hls_manifest_extension(&normalized_hls_url)
    } else {
        hls_url.to_string()
    };
    // The wrapper seals the normalized entry URL, not the provider-resolved one, so every
    // refresh can resolve it to whichever account is selected then.
    let mut entry_url = url.clone();
    // Recover archive context when callers (esp. Xtream timeshift) pass None but the
    // resolved provider URL / catchup session still carries Flussonic archive markers.
    let archive_reference = archive_reference.or_else(|| m3u_archive_epg_reference_ts(&url)).or_else(|| {
        user_session
            .map(|session| session.token.as_str())
            .or(session_token_hint)
            .and_then(m3u_catchup_epg_reference_from_session_token)
    });

    let is_live_hls = archive_reference.is_none()
        && hls_playback_kind(
            archive_reference,
            &url,
            user_session.map(|session| session.token.as_str()).or(session_token_hint),
        ) == PlaybackKind::LiveHls;

    let config_snapshot = app_state.app_config.config.load();
    let playback_id = matches!(stage, HlsRequestStage::Entry)
        .then(|| effective_playback_id_for_user(&config_snapshot, user, req_headers))
        .flatten();

    let classified = if matches!(stage, HlsRequestStage::Entry) {
        classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(virtual_id)).await
    } else {
        ResolvedStreamTarget::Item
    };
    let (entry_group, prefer_member) = FailoverGroupRecord::classify_entry_target(&classified, is_live_hls);

    // Only a fresh entry is gated. Variant refreshes, segments and entry re-polls of a running
    // session (unwrapped media playlists) belong to a running playback. A group checks
    // maintenance per member during selection.
    if matches!(stage, HlsRequestStage::Entry)
        && entry_group.is_none()
        && !is_running_playback(user_session)
        && blocks_new_playback(app_state, input, user, false)
    {
        return hls_custom_video_manifest_response(
            app_state,
            user,
            CustomVideoStreamType::Maintenance,
            StatusCode::SERVICE_UNAVAILABLE,
        )
        .await;
    }
    let hls_cache_origin = build_hls_origin_resolution(input, &url);
    let hls_origin_source = hls_cache_origin.as_ref().map(|_| {
        build_hls_origin_source_for_playback(input, stream_ref.clone(), archive_reference, Some(url.as_str()))
    });
    let server_info = app_state.app_config.get_user_server_info(user);
    let Some(server_info) = server_info else {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    };

    let disabled_headers = app_state.get_disabled_headers();
    let default_user_agent = app_state.app_config.config.load().default_user_agent.clone();
    let mut headers = build_hls_manifest_request_headers(
        &input.headers,
        req_headers,
        disabled_headers.as_ref(),
        default_user_agent.as_deref(),
        stream_context.identity().upstream_user_agent(),
    );

    let hls_cache_enabled = hls_cache_enabled_for_user(app_state, target, input, user);
    if hls_cache_enabled {
        if let Some(group) = entry_group {
            // The shared origin belongs to the selected member, so viewers of the same member share
            // it. The cache path reserves its own provider account; the selection only picks.
            let playback = group_playback_settings(app_state, target);
            let kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
            let now_ts = chrono::Utc::now().timestamp();
            let mode =
                archive_reference.map_or(SelectionMode::Live, |start_ts| SelectionMode::Archive { start_ts, now_ts });
            let req = GroupSelectionRequest {
                mode,
                prefer: prefer_member,
                allow_provider_grace: playback.allow_provider_grace,
                bypass_maintenance: user.is_recording_proxy_user(),
                allow_preempt: false,
                ..GroupSelectionRequest::new(
                    target.name.as_str(),
                    &group,
                    playback.start,
                    &fingerprint.addr,
                    &user.username,
                    connection_priority_for_kind(user, kind),
                    kind,
                )
            };
            let Ok((resolved, probed)) = super::group_start::resolve_checked_hls_group_entry(
                app_state,
                target,
                Arc::clone(&group),
                req,
                req_headers,
                false,
                None,
                true,
                fingerprint,
            )
            .await
            else {
                return hls_panel_provisioning_or_status_response(
                    app_state,
                    user,
                    input,
                    virtual_id,
                    original_hls_entry_path,
                    server_info.path.as_deref(),
                    StatusCode::SERVICE_UNAVAILABLE,
                )
                .await;
            };
            // In shared cache mode, the cache reserves its own provider account during stream startup.
            // Accepted limitation: one extra manifest probe request per fresh entry, and a potential
            // provider account change between probe and cache worker start.
            let mut selection_handle = resolved.acquired.handle;
            let allocated = selection_handle
                .handle()
                .and_then(|handle| handle.allocation.get_provider_name())
                .unwrap_or_else(|| Arc::clone(&resolved.member.input_name));
            app_state.connection_manager.release_provider_handle(selection_handle.take());

            // A live group session is keyed by the logical group. Once it exists, every viewer joins
            // it and reuses its bound member (`origin_source`), so `prefer` only affects the first
            // creation. Archive group playback keeps the per-member key below.
            let existing_group_session = if archive_reference.is_none() {
                super::group_start::shared_cache_group_session_origin(app_state, target, &group, group.group_vid.get())
                    .await
            } else {
                None
            };

            let selected_matches_bound = existing_group_session.as_ref().is_some_and(|(bound_source, _)| {
                bound_source.input_name.as_ref() == resolved.member.input_name.as_ref()
                    && bound_source.stream_ref.as_str() == resolved.member.input_stream_id.as_ref()
            });

            let (member_source, member_url, member_input) = if let Some((bound_source, bound_url)) =
                existing_group_session
            {
                if selected_matches_bound {
                    let Some(bound_input) = app_state.app_config.get_input_by_name(&bound_source.input_name) else {
                        return hls_panel_provisioning_or_status_response(
                            app_state,
                            user,
                            input,
                            virtual_id,
                            original_hls_entry_path,
                            server_info.path.as_deref(),
                            StatusCode::SERVICE_UNAVAILABLE,
                        )
                        .await;
                    };
                    (bound_source, bound_url, bound_input)
                } else {
                    // R2: a fresh selection disagreed with the bound member of a non-reusable group
                    // session. Trigger the member handoff (single-flight, coalesced with R3 triggers)
                    // and serve the freshly selected member for this entry response.
                    if probed.is_some() {
                        let session_key = HlsSessionKey::for_group(target.id, group.group_vid.get());
                        if let Some(group_session) = app_state.hls.proxy.sessions().get_by_key(&session_key).await {
                            let runtime = group_session.read().await.origin_account_runtime.clone();
                            if let Some(runtime) = runtime {
                                runtime
                                    .notify_member_unhealthy(&group_session, HlsMemberUnhealthyReason::ManifestFailure);
                            }
                        }
                    }
                    let Some(member_url) = (if let Some(ref probed) = probed {
                        Some(probed.request_url.clone())
                    } else {
                        super::group_start::build_hls_group_member_origin_url(
                            app_state,
                            target,
                            &resolved.input,
                            &resolved.member,
                            &allocated,
                            None,
                        )
                        .await
                    }) else {
                        return hls_panel_provisioning_or_status_response(
                            app_state,
                            user,
                            input,
                            virtual_id,
                            original_hls_entry_path,
                            server_info.path.as_deref(),
                            StatusCode::SERVICE_UNAVAILABLE,
                        )
                        .await;
                    };
                    let member_source = build_hls_origin_source_for_playback(
                        &resolved.input,
                        resolved.member.input_stream_id.to_string(),
                        archive_reference,
                        Some(member_url.as_str()),
                    );
                    (member_source, member_url, Arc::clone(&resolved.input))
                }
            } else {
                let Some(member_url) = (if let Some(ref probed) = probed {
                    Some(probed.request_url.clone())
                } else {
                    super::group_start::build_hls_group_member_origin_url(
                        app_state,
                        target,
                        &resolved.input,
                        &resolved.member,
                        &allocated,
                        None,
                    )
                    .await
                }) else {
                    return hls_panel_provisioning_or_status_response(
                        app_state,
                        user,
                        input,
                        virtual_id,
                        original_hls_entry_path,
                        server_info.path.as_deref(),
                        StatusCode::SERVICE_UNAVAILABLE,
                    )
                    .await;
                };
                let member_source = build_hls_origin_source_for_playback(
                    &resolved.input,
                    resolved.member.input_stream_id.to_string(),
                    archive_reference,
                    Some(member_url.as_str()),
                );
                (member_source, member_url, Arc::clone(&resolved.input))
            };

            if archive_reference.is_some() {
                return create_hls_cache_entry_master_playlist_response_with_playback_id(
                    app_state,
                    fingerprint,
                    user,
                    member_source,
                    virtual_id,
                    user_session,
                    stream_context.known_bitrate_bps(),
                    session_token_hint,
                    member_url.as_str(),
                    &member_input,
                    connection_permission,
                    connection_kind,
                    server_info.path.as_deref(),
                    playback_id,
                )
                .await;
            }

            let member_origin = build_hls_origin_resolution(&member_input, &member_url);
            return create_hls_cache_entry_master_playlist_response_with_key_and_playback_id(
                app_state,
                fingerprint,
                user,
                member_source,
                HlsSessionKey::for_group(target.id, group.group_vid.get()),
                group.group_vid.get(),
                user_session,
                stream_context.known_bitrate_bps(),
                session_token_hint,
                member_origin.as_ref().map_or(member_url.as_str(), |origin| origin.session_entry_url.as_str()),
                &member_input,
                connection_permission,
                connection_kind,
                server_info.path.as_deref(),
                playback_id,
            )
            .await;
        }
        let Some(origin_source) = hls_origin_source.clone() else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        return create_hls_cache_entry_master_playlist_response_with_playback_id(
            app_state,
            fingerprint,
            user,
            origin_source,
            virtual_id,
            user_session,
            stream_context.known_bitrate_bps(),
            session_token_hint,
            if archive_reference.is_some() {
                url.as_str()
            } else {
                hls_cache_origin.as_ref().map_or(url.as_str(), |origin| origin.session_entry_url.as_str())
            },
            input,
            connection_permission,
            connection_kind,
            server_info.path.as_deref(),
            playback_id,
        )
        .await;
    }

    let fallback_connection_kind = connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);
    let mut expected_binding = user_session.and_then(|session| session.member_binding.clone());
    let mut bound_member_info: Option<(u32, Arc<ConfigInput>, String, Option<HalfOpenGuard>)> = None;
    let mut is_stale_child_legacy_group = false;

    // Set when the session cookie for the pinned URL's host replaced the request cookie; holds the prior value.
    let mut replaced_cookie: Option<Option<axum::http::HeaderValue>> = None;
    // `pinned_stalker_stream` is set when a pinned refresh resolved its Stalker link; a rejected playlist then
    // forces one fresh `create_link` for the same account.
    let (
        mut request_url,
        session_token,
        provider_handle,
        selected_provider_config,
        provider_binding_tag,
        pinned_stalker_stream,
        probed_manifest,
        precomputed_user_agent_stream_index,
        established_identity,
    ): (String, _, _, _, _, _, _, _, _) = if let Some(session) = user_session {
        let bound_inp_opt = session.member_binding.as_ref().and_then(|binding| {
            app_state.app_config.get_input_by_name(&binding.input_name).map(|inp| (binding.member_vid, inp))
        });
        if let Some((member_vid, ref bound_inp)) = bound_inp_opt {
            // The group entry's stream ref belongs to its representative channel; a bound member
            // resolves its own portal stream and account on every Stalker refresh.
            let member_stream_ref = if bound_inp.input_type.is_stalker() {
                bound_member_portal_stream_id(app_state, target, member_vid)
                    .await
                    .map(|provider_id| provider_id.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            bound_member_info = Some((member_vid, Arc::clone(bound_inp), member_stream_ref, None));
        }
        let effective_lookup_input = bound_inp_opt.as_ref().map_or(input, |(_, inp)| inp.as_ref());
        let pinned_provider = if session.provider.is_empty() {
            effective_lookup_input.name.clone()
        } else {
            Arc::clone(&session.provider)
        };
        let pinned_kind = hls_playback_kind(archive_reference, &url, Some(session.token.as_str()));
        // A playlist is text, not media: a refresh of a running playback resolves its pinned account
        // without allocating, waiting for or failing on provider capacity. Only segments hold slots.
        let Some(selected_provider_config) = app_state.active_provider.find_provider_config(&pinned_provider) else {
            debug_if_enabled!(
                "HLS pinned provider {} is no longer configured for {}; aborting to prevent mid-session migration",
                sanitize_sensitive_info(&pinned_provider),
                sanitize_sensitive_info(&fingerprint.addr.to_string())
            );
            // An ended or rebound session must not trigger panel provisioning for the stale pinned account.
            if app_state.active_users.session_identity(&user.username, &session.token).await != Some(session.identity())
            {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                effective_lookup_input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };
        if selected_provider_config.is_account_blocked() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        // URL acceptance follows the provider that produced the sealed URL, not the session:
        // entry URLs are re-resolved to the selected account on every fetch, child URLs
        // (often CDN URLs) are fetched as sealed and only from their origin provider.
        // A Stalker URL is account-bound: refreshes resolve a fresh link, sealed variants stay with their account.
        // A group member session follows the same account-bound rules as any other Stalker session.
        let is_live_legacy_group = is_live_hls && bound_member_info.is_some();
        let selected_member_vid = bound_member_info.as_ref().map(|(member_vid, ..)| *member_vid);
        let child_decision = super::legacy::evaluate_child_manifest_source(
            stage,
            selected_provider_config.name.as_ref(),
            selected_member_vid,
            is_live_legacy_group,
        );
        is_stale_child_legacy_group = child_decision == super::legacy::StaleChildDecision::ReResolveBoundMember;
        let stalker_url = effective_lookup_input.input_type.is_stalker().then(|| {
            if is_stale_child_legacy_group {
                StalkerManifestUrl::Resolve
            } else {
                stalker_manifest_url(stage, selected_provider_config.name.as_ref())
            }
        });
        if stalker_url == Some(StalkerManifestUrl::Reject) {
            debug_if_enabled!(
                "HLS Stalker manifest token without matching origin rejected for selected provider {}",
                sanitize_sensitive_info(&selected_provider_config.name)
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        let accept_requested_stream_url = stalker_url == Some(StalkerManifestUrl::Accept)
            || match child_decision {
                super::legacy::StaleChildDecision::Accept => match stage {
                    HlsRequestStage::Variant { source: HlsManifestSource::Child, .. } => true,
                    HlsRequestStage::Entry | HlsRequestStage::LegacyVariant | HlsRequestStage::Variant { .. } => false,
                },
                super::legacy::StaleChildDecision::ReResolveBoundMember => false,
                super::legacy::StaleChildDecision::RejectNotFound => {
                    let origin_provider = match stage {
                        HlsRequestStage::Variant { origin_provider, .. } => origin_provider,
                        _ => None,
                    };
                    debug_if_enabled!(
                        "HLS child manifest from provider {} rejected for selected provider {}",
                        sanitize_sensitive_info(origin_provider.unwrap_or("<unknown>")),
                        sanitize_sensitive_info(&selected_provider_config.name)
                    );
                    return StatusCode::NOT_FOUND.into_response();
                }
            };
        let pinned_stalker_stream = (stalker_url == Some(StalkerManifestUrl::Resolve))
            .then(|| {
                let stream_ref = bound_member_info.as_ref().map_or(stream_ref.as_str(), |(_, _, member_ref, _)| {
                    if member_ref.is_empty() {
                        stream_ref.as_str()
                    } else {
                        member_ref.as_str()
                    }
                });
                stalker_hls_stream(stream_ref, archive_reference.is_some())
            })
            .flatten();
        let selected_stream = if stalker_url == Some(StalkerManifestUrl::Resolve) {
            match pinned_stalker_stream {
                Some((provider_id, kind)) => {
                    resolve_stalker_stream_for_account(
                        app_state,
                        effective_lookup_input,
                        &selected_provider_config.name,
                        provider_id,
                        kind,
                        false,
                    )
                    .await
                }
                None => None,
            }
        } else if accept_requested_stream_url {
            Some((Arc::clone(&selected_provider_config.name), url.clone()))
        } else if let Some((member_vid, member_input, _, _)) = bound_member_info.as_ref() {
            // A bound group member resolves its own playlist URL from the group record on every
            // refresh. The sealed entry URL belongs to the member at entry time and cannot be
            // mapped across inputs after a runtime member switch.
            resolve_bound_group_member_stream(app_state, target, *member_vid, member_input, &selected_provider_config)
                .await
                .map(|stream_url| (Arc::clone(&selected_provider_config.name), stream_url))
        } else {
            select_provider_stream_url(
                &url,
                effective_lookup_input,
                &selected_provider_config,
                accept_requested_stream_url,
                &app_state.app_config,
            )
            .await
        };
        let Some((_provider_name, stream_url)) = selected_stream else {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        // A session that ended or switched accounts meanwhile must not be rebound.
        let session_headers = app_state
            .active_users
            .provider_headers_for_session_identity(&user.username, &session.token, session.identity(), &stream_url)
            .await;
        if session_headers == tuliprox_session::SessionProviderHeaders::NoSession {
            debug_if_enabled!(
                "HLS session {} changed while resolving pinned provider {}",
                sanitize_sensitive_info(&session.token),
                sanitize_sensitive_info(&pinned_provider)
            );
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        if session.provider.as_ref() == selected_provider_config.name.as_ref() {
            if let tuliprox_session::SessionProviderHeaders::Headers(session_headers) = session_headers {
                replaced_cookie = Some(headers.get(axum::http::header::COOKIE).cloned());
                append_user_session_provider_headers(&mut headers, &session_headers);
            }
        }
        // The refresh may only land on the session this request resolved provider and URL for: a
        // switch that published another provider or member while the request ran must not be
        // overwritten by this request's older view. The fence is checked in the session write lock.
        let session_refresh = app_state
            .active_users
            .create_user_session_checked(
                crate::api::model::CreateUserSessionParams {
                    user,
                    session_token: &session.token,
                    virtual_id,
                    provider: &selected_provider_config.name,
                    stream_url: &stream_url,
                    addr: &fingerprint.addr,
                    connection_permission,
                    connection_kind: session.connection_kind.or(connection_kind),
                    socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                },
                Some((session.identity(), session.member_binding.as_ref())),
            )
            .await;
        let Some((session_token, post_refresh_identity)) = session_refresh else {
            debug_if_enabled!(
                "HLS session {} changed while resolving pinned provider {}; retrying",
                sanitize_sensitive_info(&session.token),
                sanitize_sensitive_info(&pinned_provider)
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let session_ttl_secs = get_hls_playback_ttl_secs(app_state, pinned_kind);
        app_state.active_provider.refresh_adaptive_playback_lease(
            &selected_provider_config.name,
            &session_token,
            pinned_kind,
            session_ttl_secs,
        );
        let provider_binding_tag = app_state.active_provider.binding_tag_for_owner(&session_token);
        (
            stream_url.clone(),
            Some(session_token),
            None,
            Some(selected_provider_config),
            provider_binding_tag,
            pinned_stalker_stream,
            None,
            None,
            Some(post_refresh_identity),
        )
    } else if let Some(group) = entry_group {
        let user_session_token = hls_entry_user_session_token(
            fingerprint,
            &user.username,
            virtual_id,
            session_token_hint,
            archive_reference,
        );
        let playback_kind = hls_playback_kind(archive_reference, &url, Some(user_session_token.as_str()));
        let hls_session_ttl_secs = get_hls_playback_ttl_secs(app_state, playback_kind);

        let playback = group_playback_settings(app_state, target);
        let now_ts = chrono::Utc::now().timestamp();
        let mode =
            archive_reference.map_or(SelectionMode::Live, |start_ts| SelectionMode::Archive { start_ts, now_ts });
        let req = GroupSelectionRequest {
            mode,
            prefer: prefer_member,
            lease: Some(PlaybackLeaseRef::new(user_session_token.as_str(), playback_kind)),
            allow_provider_grace: playback.allow_provider_grace,
            bypass_maintenance: user.is_recording_proxy_user(),
            ..GroupSelectionRequest::new(
                target.name.as_str(),
                &group,
                playback.start,
                &fingerprint.addr,
                &user.username,
                connection_priority_for_kind(user, fallback_connection_kind),
                fallback_connection_kind,
            )
        };

        let Ok((resolved, probed_manifest)) = super::group_start::resolve_checked_hls_group_entry(
            app_state,
            target,
            Arc::clone(&group),
            req,
            req_headers,
            true,
            Some((&user.username, user_session_token.as_str())),
            false,
            fingerprint,
        )
        .await
        else {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };

        let precomputed_user_agent_stream_index = probed_manifest.as_ref().and_then(|p| p.user_agent_stream_index);

        let selected_provider_config =
            resolved.acquired.handle.handle().and_then(|handle| handle.allocation.get_provider_config());
        let allocated = selected_provider_config
            .as_ref()
            .map_or_else(|| Arc::clone(&resolved.member.input_name), |cfg| Arc::clone(&cfg.name));

        let Some(member_url) = (if let Some(probed) = probed_manifest.as_ref() {
            Some(probed.request_url.clone())
        } else {
            super::group_start::build_hls_group_member_origin_url(
                app_state,
                target,
                &resolved.input,
                &resolved.member,
                &allocated,
                selected_provider_config.as_ref(),
            )
            .await
        }) else {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };

        let mut acquired_handle = resolved.acquired.handle;
        let provider_handle = acquired_handle.take();

        let provider_name = selected_provider_config
            .as_ref()
            .map_or_else(|| Arc::clone(&resolved.input.name), |cfg| Arc::clone(&cfg.name));

        let session_token = app_state
            .active_users
            .create_user_session_with_playback_id(
                crate::api::model::CreateUserSessionParams {
                    user,
                    session_token: &user_session_token,
                    virtual_id,
                    provider: &provider_name,
                    stream_url: &member_url,
                    addr: &fingerprint.addr,
                    connection_permission,
                    connection_kind: Some(fallback_connection_kind),
                    socket_bound: PlaylistItemType::LiveHls.uses_socket_bound_session(),
                },
                playback_id,
            )
            .await;

        let new_binding = MemberBinding {
            group_vid: group.group_vid.get(),
            member_vid: resolved.member.member_vid.get(),
            member_index: resolved.acquired.index,
            input_name: Arc::clone(&resolved.member.input_name),
            member_title: Arc::clone(&resolved.member.title),
            provider: Arc::clone(&provider_name),
        };
        app_state.active_users.bind_member(&user.username, &session_token, new_binding.clone()).await;
        expected_binding = Some(new_binding);

        app_state.active_provider.refresh_adaptive_playback_lease(
            &provider_name,
            &user_session_token,
            playback_kind,
            hls_session_ttl_secs,
        );

        bound_member_info = Some((
            resolved.member.member_vid.get(),
            Arc::clone(&resolved.input),
            resolved.member.input_stream_id.to_string(),
            resolved.acquired.half_open,
        ));

        debug_if_enabled!(
                "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} (group) member_vid={} provider={} stream_url={}",
                sanitize_sensitive_info(&user.username),
                resolved.member.member_vid.get(),
                selected_provider_config.as_ref().map_or("<unknown>", |provider| provider.name.as_ref()),
                sanitize_sensitive_info(&member_url)
            );

        entry_url = member_url.clone();

        let provider_binding_tag = provider_handle.as_ref().and_then(|handle| handle.binding_tag);
        let pinned_stalker_stream = if resolved.input.input_type.is_stalker() {
            Some((resolved.member.provider_id, stalker_stream_kind(resolved.member.cluster, resolved.member.item_type)))
        } else {
            None
        };
        (
            member_url,
            Some(session_token),
            provider_handle,
            selected_provider_config,
            provider_binding_tag,
            pinned_stalker_stream,
            probed_manifest,
            precomputed_user_agent_stream_index,
            None,
        )
    } else {
        // Append/shift catchup must keep an m3u-catchup session token even when shared HLS
        // cache is off; otherwise rewritten segments register as LiveHls in the panel.
        let user_session_token = hls_entry_user_session_token(
            fingerprint,
            &user.username,
            virtual_id,
            session_token_hint,
            archive_reference,
        );
        let hls_session_owner = if hls_cache_enabled {
            let session_key = HlsSessionKey::new(input.id, stream_context.stream_ref());
            let proxy_session_id = build_proxy_session_id(&session_key, &app_state.get_encrypt_secret());
            Some(build_hls_origin_session_owner(&proxy_session_id))
        } else {
            None
        };
        let session_owner = hls_session_owner.as_deref().unwrap_or(user_session_token.as_str());
        // Archive playback keeps its own reconnect window semantics, so the lease must
        // be classified as catchup rather than plain live HLS.
        let playback_kind = hls_playback_kind(archive_reference, &url, Some(user_session_token.as_str()));
        let hls_session_ttl_secs = get_hls_playback_ttl_secs(app_state, playback_kind);
        let Some(reservation) = try_reserve_hls_entry_origin_account_for_redirect(
            app_state,
            fingerprint,
            user,
            input,
            virtual_id,
            &url,
            &user_session_token,
            session_owner,
            playback_kind,
            hls_session_ttl_secs,
            connection_permission,
            fallback_connection_kind,
            true,
            playback_id,
            stalker_hls_stream(stream_context.stream_ref(), archive_reference.is_some()),
        )
        .await
        else {
            return hls_panel_provisioning_or_status_response(
                app_state,
                user,
                input,
                virtual_id,
                original_hls_entry_path,
                server_info.path.as_deref(),
                StatusCode::SERVICE_UNAVAILABLE,
            )
            .await;
        };
        debug_if_enabled!(
            "API endpoint [HLS] create_session_fingerprint user={} virtual_id={virtual_id} provider={} stream_url={}",
            sanitize_sensitive_info(&user.username),
            reservation.selected_provider_config.as_ref().map_or("<unknown>", |provider| provider.name.as_ref()),
            sanitize_sensitive_info(&reservation.request_url)
        );
        let provider_binding_tag = reservation.provider_handle.as_ref().and_then(|handle| handle.binding_tag);
        (
            reservation.request_url,
            Some(reservation.session_token),
            reservation.provider_handle,
            reservation.selected_provider_config,
            provider_binding_tag,
            None,
            None,
            None,
            None,
        )
    };

    // The session as created or reserved above; manifest response cookies may only enter this
    // binding, not one an account switch installed while the playlist downloaded.
    let mut session_identity = match established_identity {
        Some(identity) => Some(identity),
        None => match session_token.as_deref() {
            Some(token) => app_state.active_users.session_identity(&user.username, token).await,
            None => None,
        },
    };
    let provider_request_id = provider_handle.as_ref().and_then(|handle| handle.playback_request_id);
    let selected_provider_name = selected_provider_config.as_ref().map(|cfg| Arc::clone(&cfg.name));
    // A legacy failover-group session holds a member binding; the timeline rebase and the runtime
    // switch apply only to its live media playlist polls.
    let legacy_group_session = bound_member_info.is_some();
    let base_url = server_info.get_base_url();

    // The entry request already downloaded this playlist; serve it once to the immediate variant
    // request when it is still bound to the same provider binding.
    if matches!(stage, HlsRequestStage::Variant { source: HlsManifestSource::Entry, .. }) {
        if let (Some(session_token), Some(provider)) = (session_token.as_deref(), selected_provider_name.as_deref()) {
            if let Some(entry) = app_state
                .hls
                .playlist_handoff
                .take(session_token, hls_url)
                .filter(|entry| entry.matches(provider, provider_binding_tag))
            {
                app_state.connection_manager.release_provider_handle(provider_handle);
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                    .await;
                return hls_response(entry.content).into_response();
            }
        }
    }

    let (effective_virtual_id, effective_input, effective_stream_ref, half_open_guard) =
        if let Some((m_vid, ref m_input, ref m_sref, guard)) = bound_member_info {
            (m_vid, m_input.as_ref(), if m_sref.is_empty() { stream_ref.as_str() } else { m_sref.as_str() }, guard)
        } else {
            (virtual_id, input, stream_ref.as_str(), None)
        };

    let user_agent_stream_index = if let Some(stream_index) = precomputed_user_agent_stream_index {
        Some(stream_index)
    } else {
        crate::api::api_utils::resolve_stream_user_agent_index(
            app_state,
            effective_input,
            selected_provider_config.is_some(),
            &user.username,
            session_token.as_deref(),
        )
        .await
    };
    if let Some(stream_index) = user_agent_stream_index {
        request::append_user_agent_stream_index(&mut headers, stream_index);
        if let Some(session_token) = session_token.as_deref() {
            app_state
                .active_users
                .set_user_agent_stream_index_if_absent(&user.username, session_token, stream_index)
                .await;
        }
    }

    // Playlist requests only need the chosen provider account to derive the URL and pin the session.
    // Holding the provider slot until the first segment request causes stale active connections and
    // breaks forced same-account reuse on the next HLS/Catchup stream request.
    app_state.connection_manager.release_provider_handle(provider_handle);

    // A segment failure of the bound member marked it failed; the next manifest poll switches
    // without retrying the failed member. If another actor owns the timeline now (the switch
    // published a new binding or is in flight elsewhere), this request's snapshot still resolves
    // the failed member, so it answers with a retry instead of downloading that member's URL.
    if is_live_hls && legacy_group_session && user_session.is_some_and(|session| session.member_failed_on_segment) {
        if let (Some(session), Some(session_token)) = (user_session, session_token.as_deref()) {
            match try_switch_legacy_hls_member(
                app_state,
                fingerprint,
                user,
                target,
                &base_url,
                req_headers,
                session,
                session_token,
                FailureClass::Stream,
                false,
                fallback_connection_kind,
            )
            .await
            {
                LegacySwitchOutcome::Switched(response) => return response,
                // The switch already published the new member binding (lost fence, stale plan, ended
                // session) or another request holds the single-flight guard: this request's snapshot
                // still resolves the OLD, failed member, so answer instead of downloading it.
                LegacySwitchOutcome::SwitchInFlight => {
                    release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                        .await;
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                LegacySwitchOutcome::RetrySame
                | LegacySwitchOutcome::GiveUp
                | LegacySwitchOutcome::Disabled
                | LegacySwitchOutcome::Committed
                | LegacySwitchOutcome::Cooldown { .. }
                | LegacySwitchOutcome::MemberNotFound => {}
            }
        }
    }

    let mut download_result = if let Some(probed) = probed_manifest {
        Ok((probed.content, probed.response_url, probed.response_headers, probed.playlist_kind))
    } else {
        download_classified_hls_manifest(app_state, effective_input, &request_url, &headers).await
    };
    // A cached Stalker link the portal no longer honors: resolve a fresh one for the pinned account, retry once.
    if let (true, Some((provider_id, kind)), Some(provider)) =
        (is_rejected_hls_manifest_download(&download_result), pinned_stalker_stream, selected_provider_name.as_ref())
    {
        if let Some((_, fresh_url)) =
            resolve_stalker_stream_for_account(app_state, effective_input, provider, provider_id, kind, true).await
        {
            if fresh_url != request_url {
                debug_if_enabled!(
                    "HLS Stalker playlist rejected for pinned provider {}; retrying with a fresh link",
                    sanitize_sensitive_info(provider)
                );
                request_url = fresh_url;
                if let (Some(token), Some(identity)) = (session_token.as_deref(), session_identity) {
                    if !app_state
                        .active_users
                        .update_current_session_stream_url(&user.username, token, identity, &request_url)
                        .await
                    {
                        session_identity = None;
                    }
                }
                // The session cookie was chosen for the old URL's host; re-read it for the fresh URL.
                if let Some(prior_cookie) = replaced_cookie.take() {
                    let fresh_headers = match (session_token.as_deref(), session_identity) {
                        (Some(token), Some(identity)) => {
                            app_state
                                .active_users
                                .provider_headers_for_session_identity(&user.username, token, identity, &request_url)
                                .await
                        }
                        _ => tuliprox_session::SessionProviderHeaders::NoSession,
                    };
                    let fresh_headers = match &fresh_headers {
                        tuliprox_session::SessionProviderHeaders::Headers(headers) => Some(headers),
                        _ => None,
                    };
                    rebind_session_cookie(&mut headers, prior_cookie, fresh_headers);
                }
                download_result =
                    download_classified_hls_manifest(app_state, effective_input, &request_url, &headers).await;
            }
        }
    }
    let mut download_result = match download_result {
        Ok((content, response_url, response_headers, playlist_kind)) => match playlist_kind {
            // A refresh keeps its session on a malformed body; the player retries the playlist.
            HlsPlaylistKind::Invalid if stage != HlsRequestStage::Entry => {
                warn!("Upstream HLS refresh returned no playlist for virtual_id={virtual_id}; keeping session");
                if let Some(session_token) = session_token.as_deref() {
                    release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                        .await;
                }
                return StatusCode::BAD_GATEWAY.into_response();
            }
            HlsPlaylistKind::Invalid => {
                Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "upstream response is not an HLS playlist"))
            }
            playlist_kind => Ok((content, response_url, response_headers, playlist_kind)),
        },
        Err(err) => Err(err),
    };
    if is_stale_child_legacy_group {
        if let Ok((content, response_url, response_headers, HlsPlaylistKind::Master)) = &download_result {
            let target_bandwidth = stream_context.known_bitrate_bps().map(u64::from);
            // The served child replaces the master, but the master's cookies still belong to the
            // provider session; both sets are persisted below.
            let master_response_headers = response_headers.clone();
            download_result = super::legacy::resolve_stale_child_master_variant(
                app_state,
                effective_input,
                content,
                response_url,
                response_headers,
                &headers,
                target_bandwidth,
            )
            .await
            .map(|variant| super::legacy::merge_master_response_cookies(variant, &master_response_headers));
        }
    }
    match download_result {
        Ok((content, response_url, response_headers, playlist_kind)) => {
            if let Some(guard) = half_open_guard {
                app_state.member_health.record_success_with(guard);
            }
            let encrypt_secret = app_state.get_encrypt_secret();
            // A live legacy group media playlist continues the client timeline and feeds the
            // switch budget with every successful serve.
            let mut rebase_props = None;
            if legacy_group_session && is_live_hls && playlist_kind == HlsPlaylistKind::Media {
                if let Some(session_token) = session_token.as_deref() {
                    // Plan and commit the timeline under the session write lock, so a response built
                    // from this request's snapshot cannot overwrite a commit made in between. The
                    // snapshot's offset travels with its generation: a restart commit rewrites the
                    // offset, so a window rank only makes sense against the offset it was fetched for.
                    let observed_rebase = user_session.and_then(|session| session.legacy_rebase.as_ref());
                    let observed_failure_generation =
                        user_session.map_or(0, |session| session.member_failure_generation);
                    let mut committed = false;
                    // A legacy group session is always bound to a member; the update is fenced on
                    // that binding and on the identity read for this request.
                    if let (Some(identity), Some(expected_binding)) = (session_identity, expected_binding.as_ref()) {
                        // The window facts are parsed before the session write lock: the scan must
                        // not run while the global `ActiveUserManager` lock is held.
                        let window = LegacyMediaWindow::parse(&content, playlist_kind);
                        let playback = group_playback_settings(app_state, target);
                        let has_segments = window.segment_count > 0;
                        let update = app_state
                            .active_users
                            .commit_legacy_media_serve_if_bound(
                                &user.username,
                                session_token,
                                identity,
                                expected_binding,
                                observed_failure_generation,
                                |state, budget_slot| {
                                    let plan = plan_legacy_rebase(state, window, playlist_kind, observed_rebase);
                                    if playback.failover.enabled {
                                        // Only a window that is served and contains segments counts
                                        // as playback for the failover budget; an empty one is not data.
                                        if matches!(plan, LegacyRebasePlan::Serve(_)) && has_segments {
                                            budget_slot
                                                .get_or_insert_with(|| FailoverBudget::new(&playback.failover))
                                                .on_data(std::time::Instant::now());
                                        }
                                    } else {
                                        *budget_slot = None;
                                    }
                                    plan
                                },
                                |plan| matches!(plan, LegacyRebasePlan::Serve(_)) && has_segments,
                            )
                            .await;
                        if let Some(LegacyRebasePlan::Serve(props)) = update {
                            rebase_props = props;
                            committed = true;
                        }
                    }
                    // Commit only onto the binding this manifest was fetched for. A concurrent
                    // switch or rebind owns the timeline now; the player retries the playlist.
                    if !committed {
                        release_prepared_hls_manifest_session(
                            app_state,
                            &user.username,
                            session_token,
                            &fingerprint.addr,
                        )
                        .await;
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                }
            }
            let rewrite_hls_props = RewriteHlsProps {
                secret: &encrypt_secret,
                base_url: &base_url,
                content: &content,
                hls_url: response_url,
                target_id: target.id,
                virtual_id: effective_virtual_id,
                input_id: effective_input.id,
                user_token: session_token.as_deref(),
                origin_provider: selected_provider_name.as_deref(),
                origin_member_vid: legacy_group_session.then_some(effective_virtual_id),
                playlist_kind: Some(playlist_kind),
                rebase: rebase_props,
            };
            let hls_content = rewrite_hls(user, &rewrite_hls_props);
            if let Some(session_token) = session_token.as_deref() {
                let session_headers = extract_hls_provider_session_headers(&response_headers);
                if let (false, Some(identity)) = (session_headers.is_empty(), session_identity) {
                    app_state
                        .active_users
                        .update_current_session_provider_response_headers_from(
                            &user.username,
                            session_token,
                            identity,
                            &session_headers,
                            &rewrite_hls_props.hls_url,
                        )
                        .await;
                }
                release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr)
                    .await;
            }
            if let (HlsRequestStage::Entry, HlsPlaylistKind::Media, Some(session_token), Some(provider)) =
                (stage, playlist_kind, session_token.as_deref(), selected_provider_name.as_ref())
            {
                if hls_media_playlist_wrap_enabled(app_state, target, effective_input, user) {
                    let master = wrap_media_playlist(
                        HlsMediaPlaylistWrap {
                            app_state,
                            user,
                            base_url: &base_url,
                            target_id: target.id,
                            input: effective_input,
                            virtual_id: effective_virtual_id,
                            session_token,
                            sealed_url: &entry_url,
                            provider,
                            binding_tag: provider_binding_tag,
                            known_bitrate_bps: stream_context.known_bitrate_bps(),
                            stream_ref: Some(effective_stream_ref),
                        },
                        hls_content,
                    )
                    .await;
                    return hls_response(master).into_response();
                }
            }
            hls_response(hls_content).into_response()
        }
        Err(err) => {
            // A hard failure of the bound member switches a running legacy group playback to the
            // next member and answers this poll with the rebased manifest.
            if is_live_hls && legacy_group_session {
                let class = super::group_start::classify_hls_download_error(&err);
                if is_legacy_manifest_failover_class(class) {
                    if let (Some(session), Some(session_token)) = (user_session, session_token.as_deref()) {
                        match try_switch_legacy_hls_member(
                            app_state,
                            fingerprint,
                            user,
                            target,
                            &base_url,
                            req_headers,
                            session,
                            session_token,
                            class,
                            true,
                            fallback_connection_kind,
                        )
                        .await
                        {
                            LegacySwitchOutcome::Switched(response) => return response,
                            // Another actor owns the timeline now, or the budget refuses to reuse the
                            // same member: this request's snapshot resolves a member it may not
                            // download, so answer with a retry instead.
                            LegacySwitchOutcome::RetrySame | LegacySwitchOutcome::SwitchInFlight => {
                                release_prepared_hls_manifest_session(
                                    app_state,
                                    &user.username,
                                    session_token,
                                    &fingerprint.addr,
                                )
                                .await;
                                return StatusCode::BAD_GATEWAY.into_response();
                            }
                            LegacySwitchOutcome::GiveUp
                            | LegacySwitchOutcome::Disabled
                            | LegacySwitchOutcome::Committed
                            | LegacySwitchOutcome::Cooldown { .. }
                            | LegacySwitchOutcome::MemberNotFound => {}
                        }
                    }
                }
            }
            if let Some(guard) = half_open_guard {
                app_state.member_health.record_classified_failure_with(
                    guard,
                    FailureClass::Stream,
                    std::time::Instant::now(),
                );
            }
            error!("Failed to download m3u8: {}", request::text_response_error_log_label(&err));
            if let Some(session_token) = session_token.as_deref() {
                terminate_failed_hls_manifest_session(
                    app_state,
                    &user.username,
                    session_token,
                    selected_provider_name.as_ref(),
                    provider_binding_tag,
                    provider_request_id,
                )
                .await;
            }

            hls_custom_video_manifest_response(
                app_state,
                user,
                CustomVideoStreamType::ChannelUnavailable,
                StatusCode::NOT_FOUND,
            )
            .await
        }
    }
}

/// Whether a legacy manifest download failure triggers a runtime member switch: stream errors
/// (404/410/5xx) and transport errors. Account (401/403) and capacity (429) errors keep the
/// existing per-account behavior.
fn is_legacy_manifest_failover_class(class: FailureClass) -> bool {
    matches!(class, FailureClass::Stream | FailureClass::Transport)
}

/// Upstream facts of one legacy group media playlist window.
///
/// Reading them walks the whole playlist (up to the 2 MiB download limit), so callers parse them
/// before they take the `ActiveUserManager` write lock and the planner then runs under the lock
/// without scanning the body. A playlist of any other kind carries no media window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LegacyMediaWindow {
    media_sequence: u64,
    segment_count: u64,
    discontinuity_sequence: u64,
}

impl LegacyMediaWindow {
    fn parse(content: &str, playlist_kind: HlsPlaylistKind) -> Self {
        if playlist_kind != HlsPlaylistKind::Media {
            return Self::default();
        }
        Self {
            media_sequence: media_playlist_media_sequence(content).unwrap_or(0),
            segment_count: media_playlist_segment_count(content),
            discontinuity_sequence: media_playlist_discontinuity_sequence(content).unwrap_or(0),
        }
    }
}

/// Outcome of planning one serve for a legacy group media playlist.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LegacyRebasePlan {
    /// Serve the playlist, rewritten with the optional client timeline rebase.
    Serve(Option<HlsMediaSequenceRebase>),
    /// The response is older than the timeline another request already committed; it must not be
    /// served, because re-tagging it would re-tag already served media and could drop a boundary.
    Stale,
}

/// Client timeline rebase for one media playlist serve, updating the per-session state in place.
///
/// `snapshot` is the session's rebase state as the caller read it before downloading the
/// playlist (`None` when the session had none). When its `commit_generation` differs, another
/// request already committed a newer timeline; a window that is older than that commit, or that
/// was fetched against a mapping since rewritten, must not be remapped as new media.
fn plan_legacy_rebase(
    state: &mut LegacyHlsRebase,
    window: LegacyMediaWindow,
    playlist_kind: HlsPlaylistKind,
    snapshot: Option<&LegacyHlsRebase>,
) -> LegacyRebasePlan {
    if playlist_kind != HlsPlaylistKind::Media {
        return LegacyRebasePlan::Serve(None);
    }
    let generation = state.commit_generation;
    let (plan, changed) = plan_legacy_rebase_media(state, window, snapshot);
    // Only a served window advances the committed timeline; a stale response leaves it untouched.
    if matches!(plan, LegacyRebasePlan::Serve(_)) && changed {
        state.commit_generation = generation.saturating_add(1);
    }
    plan
}

/// Applies one upstream media window to `state` and reports whether that changed the stored
/// timeline. A window that leaves the timeline untouched must not advance the commit generation.
fn plan_legacy_rebase_media(
    state: &mut LegacyHlsRebase,
    window: LegacyMediaWindow,
    snapshot: Option<&LegacyHlsRebase>,
) -> (LegacyRebasePlan, bool) {
    let before = *state;
    let plan = plan_legacy_rebase_window(state, window, snapshot);
    (plan, *state != before)
}

/// Applies the media playlist rules of one upstream window to `state` and returns how to answer
/// it.
fn plan_legacy_rebase_window(
    state: &mut LegacyHlsRebase,
    window: LegacyMediaWindow,
    snapshot: Option<&LegacyHlsRebase>,
) -> LegacyRebasePlan {
    let (observed_generation, observed_media_sequence_offset) =
        snapshot.map_or((0, 0), |observed| (observed.commit_generation, observed.media_sequence_offset));
    let upstream_seq = window.media_sequence;
    let segment_count = window.segment_count;
    let upstream_discontinuity_sequence = window.discontinuity_sequence;
    // An empty media playlist carries no window. It must not touch the timeline bookkeeping: in
    // particular it must not reset `upstream_discontinuity_sequence` to the absent tag's 0, which
    // would make the next real window add the whole upstream counter to the client counter. The
    // client keeps the timeline of the last served window, exactly like after a member switch.
    if segment_count == 0 {
        return LegacyRebasePlan::Serve(Some(HlsMediaSequenceRebase {
            media_sequence: if state.pending_member_rebase {
                state.last_served_sequence.saturating_add(1)
            } else {
                state.last_served_start
            },
            insert_discontinuity: false,
            discontinuity_sequence: state.discontinuity_sequence,
        }));
    }
    let pending_member_rebase = state.pending_member_rebase;
    if state.pending_member_rebase {
        let next_seq = state.last_served_sequence.saturating_add(1);
        let offset = i128::from(next_seq) - i128::from(upstream_seq);
        state.media_sequence_offset = i64::try_from(offset).unwrap_or(if offset > 0 { i64::MAX } else { i64::MIN });
        begin_legacy_discontinuity(state);
        state.pending_member_rebase = false;
    }
    let mut served_seq = upstream_seq.saturating_add_signed(state.media_sequence_offset);
    // A stale response for an older upstream window must not overwrite the timeline a concurrent
    // request already committed: serve it through the current mapping, without a new discontinuity.
    // A genuine restart (generation unchanged) still rebases below.
    //
    // A restart commit also rewrites `media_sequence_offset` while a slower request is downloading.
    // That request's window was fetched from the pre-restart upstream and ranked against the old
    // offset, so under the new one it maps *ahead* of the committed tail instead of behind it; the
    // window test alone would serve it and consume the committed discontinuity boundary. Any
    // changed offset therefore makes a response from an older generation stale.
    //
    // A window that starts before the committed window start is older than what the client
    // already played (its tail can reach the committed tail, as an older window ending there
    // does), so it is stale. A window whose mapped end is before the committed tail is older for
    // the same reason: an origin that grows its window in place re-serves the same start with a
    // shorter tail from an older snapshot, and serving it would re-tag already served media and
    // move the committed tail back. An identical re-serve of the committed window starts at
    // `last_served_start`, ends at `last_served_sequence`, and is harmless: it is served, not
    // rejected.
    let older_than_commit = served_seq < state.last_served_start;
    let ends_before_commit = served_seq.saturating_add(segment_count - 1) < state.last_served_sequence;
    let offset_since_snapshot = observed_media_sequence_offset != state.media_sequence_offset;
    if !pending_member_rebase
        && observed_generation != state.commit_generation
        && (older_than_commit || ends_before_commit || offset_since_snapshot)
    {
        // Another request committed a newer timeline while this one was downloading. The window is
        // older than the committed one, or was mapped by an offset that commit replaced; serving it
        // would re-tag media the client already saw (and drop the discontinuity a switch inserted);
        // the caller retries instead.
        return LegacyRebasePlan::Stale;
    }
    // An upstream restart moves the whole window behind what the client already played. Rebase it
    // after the last served segment with a discontinuity, as after a member switch.
    if served_seq.saturating_add(segment_count - 1) < state.last_served_sequence {
        let next_seq = state.last_served_sequence.saturating_add(1);
        state.media_sequence_offset =
            i64::try_from(next_seq).unwrap_or(i64::MAX).saturating_sub(i64::try_from(upstream_seq).unwrap_or(i64::MAX));
        begin_legacy_discontinuity(state);
        served_seq = next_seq;
    }
    // The first serve after a boundary comes from a restarted or different upstream whose
    // discontinuity counter is unrelated; only advances within one upstream carry over.
    let new_upstream = state.insert_discontinuity && state.discontinuity_boundary_seq.is_none();
    match state.upstream_discontinuity_sequence {
        Some(previous) if !new_upstream => {
            state.discontinuity_sequence =
                state.discontinuity_sequence.saturating_add(upstream_discontinuity_sequence.saturating_sub(previous));
        }
        None if !new_upstream => state.discontinuity_sequence = upstream_discontinuity_sequence,
        _ => {}
    }
    state.upstream_discontinuity_sequence = Some(upstream_discontinuity_sequence);
    if new_upstream {
        state.discontinuity_boundary_seq = Some(served_seq);
    }
    // The boundary tag stays on every serve while its segment is in the window; the sequence
    // advances on the serve where it leaves.
    if state.discontinuity_boundary_seq.is_some_and(|boundary| served_seq > boundary) {
        state.discontinuity_sequence = state.discontinuity_sequence.saturating_add(1);
        state.discontinuity_boundary_seq = None;
        state.insert_discontinuity = false;
    }
    let insert_discontinuity = state.insert_discontinuity;
    state.last_served_start = served_seq;
    state.last_served_sequence = served_seq.saturating_add(segment_count - 1);
    LegacyRebasePlan::Serve(
        if insert_discontinuity
            || state.media_sequence_offset != 0
            || state.discontinuity_sequence != upstream_discontinuity_sequence
        {
            Some(HlsMediaSequenceRebase {
                media_sequence: served_seq,
                insert_discontinuity,
                discontinuity_sequence: state.discontinuity_sequence,
            })
        } else {
            None
        },
    )
}

/// Starts a new discontinuity boundary for the next serve. A previous boundary still pending has
/// left the window, because the new window starts after every segment served so far.
fn begin_legacy_discontinuity(state: &mut LegacyHlsRebase) {
    if state.discontinuity_boundary_seq.take().is_some() {
        state.discontinuity_sequence = state.discontinuity_sequence.saturating_add(1);
    }
    state.insert_discontinuity = true;
}

/// Owns the provider slot acquired while a legacy member switch resolves its new member.
///
/// Every path that ends the switch without handing the slot on releases it through the
/// connection manager, which also wakes the capacity waiters of that provider. A bare drop would
/// free the slot silently, so waiters would sleep until their own deadline.
struct LegacySwitchSlot<'a> {
    connection_manager: &'a ConnectionManager,
    handle: Option<ManagedProviderHandle>,
}

impl<'a> LegacySwitchSlot<'a> {
    fn new(connection_manager: &'a ConnectionManager, handle: ManagedProviderHandle) -> Self {
        Self { connection_manager, handle: Some(handle) }
    }

    fn provider_handle(&self) -> Option<&tuliprox_core::model::ProviderHandle> {
        self.handle.as_ref().and_then(ManagedProviderHandle::handle)
    }

    /// Releases the slot now; `Drop` then has nothing left to do.
    fn release(&mut self) { self.connection_manager.release_managed_provider_handle(self.handle.take()); }
}

impl Drop for LegacySwitchSlot<'_> {
    fn drop(&mut self) { self.release(); }
}

/// Result of an attempted legacy group member switch.
#[derive(Debug)]
pub(crate) enum LegacySwitchOutcome {
    Switched(axum::response::Response),
    RetrySame,
    SwitchInFlight,
    GiveUp,
    Disabled,
    Committed,
    Cooldown { retry_after: std::time::Duration },
    MemberNotFound,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum LegacySwitchTrigger {
    Automatic { failed_index: usize, failure_class: FailureClass, record_failure: bool },
    Manual { target_index: usize },
}

/// Switches a running legacy HLS group playback to the next member after a hard failure, rebinds
/// the user session, and serves the new member's manifest (rebased). Returns `GiveUp` when the
/// switch budget is exhausted or no candidate member can serve.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn switch_legacy_hls_group_member(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    base_url: Option<&str>,
    req_headers: &HeaderMap,
    group: Arc<FailoverGroupRecord>,
    session: &UserSession,
    session_token: &str,
    trigger: LegacySwitchTrigger,
    playback: shared::model::FailoverGroupPlayback,
    connection_kind: crate::api::model::ConnectionKind,
) -> LegacySwitchOutcome {
    let now = std::time::Instant::now();

    let exclude: Vec<usize> = match trigger {
        LegacySwitchTrigger::Automatic { failed_index, failure_class, record_failure } => {
            if !playback.failover.enabled {
                return LegacySwitchOutcome::Disabled;
            }

            // A one-member group cannot switch; keep the current member and its reservation so the
            // existing error/terminal response is served without dropping the lease.
            if group.members.len() <= 1 {
                if record_failure {
                    if let Some(member) = group.members.first() {
                        record_member_failure(&app_state.member_health, member, None, failure_class);
                    }
                }
                return LegacySwitchOutcome::GiveUp;
            }

            if record_failure {
                if let Some(member) = group.members.get(failed_index) {
                    record_member_failure(&app_state.member_health, member, None, failure_class);
                }
            }

            // Start the incident on the stored budget under the session write lock. The request snapshot
            // supplies the fence and the timeline this request was fetched against, so a concurrent media
            // commit is never overwritten.
            let Some(session_binding) = session.member_binding.as_ref() else {
                return LegacySwitchOutcome::GiveUp;
            };
            let Some(decision) = app_state
                .active_users
                .update_legacy_failover_state_if_bound(
                    &user.username,
                    session_token,
                    session.identity(),
                    session_binding,
                    |_, budget_slot| {
                        budget_slot.get_or_insert_with(|| FailoverBudget::new(&playback.failover)).on_trigger(
                            failed_index,
                            group.members.len(),
                            now,
                        )
                    },
                )
                .await
            else {
                // The session ended or another request rebound it; that actor owns the switch now.
                return LegacySwitchOutcome::SwitchInFlight;
            };
            match decision {
                BudgetDecision::GiveUp => return LegacySwitchOutcome::GiveUp,
                BudgetDecision::RetrySame => return LegacySwitchOutcome::RetrySame,
                BudgetDecision::Next { exclude } => exclude,
            }
        }
        LegacySwitchTrigger::Manual { target_index } => {
            if target_index >= group.members.len() {
                return LegacySwitchOutcome::MemberNotFound;
            }
            if session.member_binding.as_ref().is_some_and(|b| b.member_index == target_index) {
                return LegacySwitchOutcome::Committed;
            }
            if let Some(last) = session.legacy_rebase.as_ref().and_then(|r| r.last_manual_switch) {
                let tokio_now = tokio::time::Instant::now();
                let elapsed = tokio_now.saturating_duration_since(last);
                if elapsed < crate::api::api_utils::failover_group_runtime::MANUAL_SWITCH_COOLDOWN {
                    let retry_after =
                        crate::api::api_utils::failover_group_runtime::MANUAL_SWITCH_COOLDOWN.saturating_sub(elapsed);
                    return LegacySwitchOutcome::Cooldown { retry_after };
                }
            }
            (0..group.members.len()).filter(|&i| i != target_index).collect()
        }
    };

    let observed_rebase = session.legacy_rebase.as_ref();
    let Some(session_binding) = session.member_binding.as_ref() else {
        return LegacySwitchOutcome::GiveUp;
    };

    // Select the next member, excluding the failed one, and probe its manifest. The new
    // reservation replaces the old one atomically in `begin_owner`; releasing the old lease
    // first would drop it with no replacement when no candidate remains.
    let req = GroupSelectionRequest {
        mode: SelectionMode::Live,
        prefer: match trigger {
            LegacySwitchTrigger::Automatic { .. } => None,
            LegacySwitchTrigger::Manual { target_index } => Some(target_index),
        },
        lease: Some(PlaybackLeaseRef::new(session_token, PlaybackKind::LiveHls)),
        allow_provider_grace: playback.allow_provider_grace,
        bypass_maintenance: user.is_recording_proxy_user(),
        exclude: &exclude,
        demoted_inputs: &[],
        allow_preempt: false,
        ..GroupSelectionRequest::new(
            target.name.as_str(),
            &group,
            playback.start,
            &fingerprint.addr,
            &user.username,
            connection_priority_for_kind(user, connection_kind),
            connection_kind,
        )
    };
    let Ok((resolved, probed)) = super::group_start::resolve_checked_hls_group_entry(
        app_state,
        target,
        Arc::clone(&group),
        req,
        req_headers,
        true,
        None,
        false,
        fingerprint,
    )
    .await
    else {
        return LegacySwitchOutcome::GiveUp;
    };
    // `check_active_cache = false` above makes the probe mandatory: `resolve_checked_hls_group_entry`
    // returns `Ok` only together with the verified manifest. Take it here, before the switch
    // publishes a binding or takes a slot, so the impossible `None` gives up with no state to unwind.
    let Some(probed) = probed else {
        debug_assert!(false, "a mandatory HLS group probe resolved without a manifest");
        return LegacySwitchOutcome::GiveUp;
    };

    let mut slot = LegacySwitchSlot::new(&app_state.connection_manager, resolved.acquired.handle);
    let selected_provider_config = slot.provider_handle().and_then(|handle| handle.allocation.get_provider_config());
    // The switch publishes the URL the manifest was verified against.
    let member_url = probed.request_url.clone();

    let provider_name = selected_provider_config
        .as_ref()
        .map_or_else(|| Arc::clone(&resolved.member.input_name), |cfg| Arc::clone(&cfg.name));

    // Rebind the user session to the new member, publish the pending rebase marker and count the
    // switch in one write under the session lock.
    let new_binding = MemberBinding {
        group_vid: group.group_vid.get(),
        member_vid: resolved.member.member_vid.get(),
        member_index: resolved.acquired.index,
        input_name: Arc::clone(&resolved.member.input_name),
        member_title: Arc::clone(&resolved.member.title),
        provider: Arc::clone(&provider_name),
    };
    let Some(switched_identity) = app_state
        .active_users
        .switch_legacy_group_member_binding(
            &user.username,
            session_token,
            session.identity(),
            session_binding,
            new_binding.clone(),
            Arc::from(member_url.as_str()),
            |budget_slot| {
                if let LegacySwitchTrigger::Automatic { failed_index, .. } = trigger {
                    // Count the switch in the same write that publishes the binding, so a lost fence on
                    // the plan update below cannot lose the accounting of a switch that happened.
                    budget_slot.get_or_insert_with(|| FailoverBudget::new(&playback.failover)).on_switched(
                        resolved.acquired.index,
                        failed_index,
                        now,
                    );
                }
            },
        )
        .await
    else {
        // The trigger's session ended or was recreated while the switch resolved its member (its
        // fence no longer matches), so there is nothing to bridge. Releasing the slot wakes the
        // capacity waiters of the acquired account.
        slot.release();
        return LegacySwitchOutcome::SwitchInFlight;
    };
    app_state.active_provider.refresh_adaptive_playback_lease(
        &provider_name,
        session_token,
        PlaybackKind::LiveHls,
        get_hls_playback_ttl_secs(app_state, PlaybackKind::LiveHls),
    );

    if matches!(trigger, LegacySwitchTrigger::Manual { .. }) {
        app_state
            .active_users
            .update_legacy_failover_state_if_bound(
                &user.username,
                session_token,
                switched_identity,
                &new_binding,
                |state, _budget_slot| {
                    state.last_manual_switch = Some(tokio::time::Instant::now());
                    LegacyRebasePlan::Serve(None)
                },
            )
            .await;
        slot.release();
        return LegacySwitchOutcome::Committed;
    }

    slot.release();

    let (content, response_url, response_headers, playlist_kind) =
        (probed.content, probed.response_url, probed.response_headers, probed.playlist_kind);

    // The window facts are parsed before the session write lock: the scan must not run while the
    // global `ActiveUserManager` lock is held.
    let window = LegacyMediaWindow::parse(&content, playlist_kind);
    // Bridge the timeline to the new member under the session write lock, fenced on the exact
    // session identity the switch published. A provider revert bumps the binding generation and
    // therefore fails this fence; a fresh read here could not detect it. The plan sees the stored
    // state, so a concurrent media commit on the new member is preserved instead of overwritten.
    let Some(rebase_plan) = app_state
        .active_users
        .update_legacy_failover_state_if_bound(
            &user.username,
            session_token,
            switched_identity,
            &new_binding,
            |state, _budget_slot| plan_legacy_rebase(state, window, playlist_kind, observed_rebase),
        )
        .await
    else {
        return LegacySwitchOutcome::SwitchInFlight;
    };
    let LegacyRebasePlan::Serve(rebase_props) = rebase_plan else {
        // A concurrent serve committed a newer window; the caller answers with a retry.
        return LegacySwitchOutcome::SwitchInFlight;
    };

    let encrypt_secret = app_state.get_encrypt_secret();
    let rewrite_hls_props = RewriteHlsProps {
        secret: &encrypt_secret,
        base_url: base_url.unwrap_or(""),
        content: &content,
        hls_url: response_url,
        target_id: target.id,
        virtual_id: resolved.member.member_vid.get(),
        input_id: resolved.input.id,
        user_token: Some(session_token),
        origin_provider: Some(&provider_name),
        origin_member_vid: Some(resolved.member.member_vid.get()),
        playlist_kind: Some(playlist_kind),
        rebase: rebase_props,
    };
    let hls_content = rewrite_hls(user, &rewrite_hls_props);

    let session_headers = extract_hls_provider_session_headers(&response_headers);
    if !session_headers.is_empty() {
        app_state
            .active_users
            .update_current_session_provider_response_headers_from(
                &user.username,
                session_token,
                switched_identity,
                &session_headers,
                &rewrite_hls_props.hls_url,
            )
            .await;
    }
    release_prepared_hls_manifest_session(app_state, &user.username, session_token, &fingerprint.addr).await;

    LegacySwitchOutcome::Switched(hls_response(hls_content).into_response())
}

/// Attempts a legacy runtime member switch and serves the new member's manifest, or returns
/// an outcome indicating why no switch happened (budget exhausted, in flight, disabled, or not a group).
#[allow(clippy::too_many_arguments)]
async fn try_switch_legacy_hls_member(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    base_url: &str,
    req_headers: &HeaderMap,
    session: &UserSession,
    session_token: &str,
    failure_class: FailureClass,
    record_failure: bool,
    connection_kind: crate::api::model::ConnectionKind,
) -> LegacySwitchOutcome {
    let Some(binding) = session.member_binding.clone() else {
        return LegacySwitchOutcome::Disabled;
    };
    let ResolvedStreamTarget::Group(group) =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(binding.group_vid))
            .await
    else {
        return LegacySwitchOutcome::Disabled;
    };
    // A removed/reordered member after a config reload must never index out of bounds.
    let Some(failed_index) = group
        .members
        .iter()
        .position(|member| member.member_vid.get() == binding.member_vid)
        .or_else(|| (binding.member_index < group.members.len()).then_some(binding.member_index))
    else {
        return LegacySwitchOutcome::GiveUp;
    };
    let playback = group_playback_settings(app_state, target);
    if !playback.failover.enabled {
        return LegacySwitchOutcome::Disabled;
    }

    // Single-flight: only one concurrent poll switches the session. The flag is shared across
    // session clones via its `Arc`, and the guard resets it even if the switch future is dropped.
    if session.legacy_switch_in_flight.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return LegacySwitchOutcome::SwitchInFlight;
    }
    let _guard = LegacySwitchGuard { flag: Arc::clone(&session.legacy_switch_in_flight) };
    switch_legacy_hls_group_member(
        app_state,
        fingerprint,
        user,
        target,
        Some(base_url),
        req_headers,
        group,
        session,
        session_token,
        LegacySwitchTrigger::Automatic { failed_index, failure_class, record_failure },
        playback,
        connection_kind,
    )
    .await
}

/// Manually switches a running legacy HLS group session to an explicit target member.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn manual_switch_legacy_hls_group_session(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    req_headers: &HeaderMap,
    group: Arc<FailoverGroupRecord>,
    session: &UserSession,
    session_token: &str,
    target_member_index: usize,
) -> LegacySwitchOutcome {
    if session.legacy_switch_in_flight.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return LegacySwitchOutcome::SwitchInFlight;
    }
    let _guard = LegacySwitchGuard { flag: Arc::clone(&session.legacy_switch_in_flight) };
    let playback = group_playback_settings(app_state, target);
    let connection_kind = session.connection_kind.unwrap_or(crate::api::model::ConnectionKind::Normal);

    switch_legacy_hls_group_member(
        app_state,
        fingerprint,
        user,
        target,
        None,
        req_headers,
        group,
        session,
        session_token,
        LegacySwitchTrigger::Manual { target_index: target_member_index },
        playback,
        connection_kind,
    )
    .await
}

/// Resets the per-session legacy switch guard on drop, including when a switch future is
/// cancelled by a client disconnect.
struct LegacySwitchGuard {
    flag: Arc<AtomicBool>,
}

impl Drop for LegacySwitchGuard {
    fn drop(&mut self) { self.flag.store(false, Ordering::Release); }
}

/// Resolves the playlist URL of the bound group member on a refresh, from the group record.
/// The sealed entry URL belongs to the member at entry time and cannot be mapped across inputs
/// after a runtime member switch.
async fn resolve_bound_group_member_stream(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    member_vid: u32,
    member_input: &Arc<ConfigInput>,
    selected_provider_config: &Arc<crate::model::ProviderConfig>,
) -> Option<String> {
    let classified =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(member_vid)).await;
    let ResolvedStreamTarget::Member { group, index } = classified else { return None };
    let member = &group.members[index];
    let allocated = Arc::clone(&selected_provider_config.name);
    super::group_start::build_hls_group_member_origin_url(
        app_state,
        target,
        member_input,
        member,
        &allocated,
        Some(selected_provider_config),
    )
    .await
}

/// Portal stream id of the failover group member a session is bound to. The group entry's own
/// stream ref belongs to the representative channel, not to the member that actually plays.
async fn bound_member_portal_stream_id(
    app_state: &Arc<AppState>,
    target: &ConfigTarget,
    member_vid: u32,
) -> Option<u32> {
    match classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(member_vid)).await {
        ResolvedStreamTarget::Member { group, index } => group.members.get(index).map(|member| member.provider_id),
        _ => None,
    }
}

/// Replaces the session cookie chosen for an earlier URL: restores the request cookie it replaced, then applies the
/// session cookie valid for the new URL, if any. Other headers stay unchanged.
fn rebind_session_cookie(
    headers: &mut HeaderMap,
    prior_cookie: Option<axum::http::HeaderValue>,
    session_headers: Option<&std::collections::HashMap<String, String>>,
) {
    match prior_cookie {
        Some(cookie) => headers.insert(axum::http::header::COOKIE, cookie),
        None => headers.remove(axum::http::header::COOKIE),
    };
    if let Some(session_headers) = session_headers {
        append_user_session_provider_headers(headers, session_headers);
    }
}

/// Playlist body, final response URL, response headers and playlist kind of a manifest download.
type ClassifiedHlsManifest = (String, String, HeaderMap, HlsPlaylistKind);

/// True when a manifest download indicates an expired or rejected link (4xx client error or invalid body).
pub(in crate::api::endpoints::hls_api) fn hls_download_rejects_link(
    result: Result<HlsPlaylistKind, &std::io::Error>,
) -> bool {
    match result {
        Ok(kind) => kind == HlsPlaylistKind::Invalid,
        Err(err) => request::request_failed_status(&err.to_string()).is_some_and(|status| {
            status.is_client_error()
                && !matches!(
                    status,
                    StatusCode::TOO_MANY_REQUESTS
                        | StatusCode::REQUEST_TIMEOUT
                        | StatusCode::PROXY_AUTHENTICATION_REQUIRED
                )
        }),
    }
}

/// A 4xx response or a non-playlist body, the two ways a portal answers an expired link.
/// Timeouts, 5xx and transport errors do not trigger a fresh `create_link`. Neither do 4xx statuses that do not
/// reject the link: rate limiting (429), request timeout (408) and proxy authentication (407).
pub(in crate::api::endpoints::hls_api) fn is_rejected_hls_manifest_download(
    result: &Result<ClassifiedHlsManifest, std::io::Error>,
) -> bool {
    hls_download_rejects_link(result.as_ref().map(|(_, _, _, kind)| *kind))
}

/// Downloads a legacy HLS playlist and classifies its body.
async fn download_classified_hls_manifest(
    app_state: &Arc<AppState>,
    input: &ConfigInput,
    request_url: &str,
    headers: &HeaderMap,
) -> Result<ClassifiedHlsManifest, std::io::Error> {
    let input_source = InputSource::for_request_url(input, request_url.to_string());
    let (content, response_url, response_headers) =
        download_legacy_hls_manifest(app_state, &input_source, headers).await?;
    let playlist_kind = classify_hls_playlist(&content);
    Ok((content, response_url, response_headers, playlist_kind))
}

/// Stream channel of `virtual_id` in the target's Xtream output, else its M3U output.
pub(in crate::api::endpoints::hls_api) async fn get_stream_channel(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
) -> Option<StreamChannel> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(pli) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            return Some(pli.to_stream_channel(target.id));
        }
    }
    let target_id = target.id;
    if let Ok(pli) = m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target).await {
        return Some(pli.to_stream_channel(target_id));
    }
    let classified =
        classify_virtual_id(&app_state.app_config, &app_state.playlists, target, VirtualId::new(virtual_id)).await;
    match classified {
        ResolvedStreamTarget::Member { group, index } => {
            let member = &group.members[index];
            Some(StreamChannel {
                target_id,
                virtual_id: member.member_vid.get(),
                group_virtual_id: Some(group.group_vid.get()),
                provider_id: member.provider_id,
                input_name: Arc::clone(&member.input_name),
                item_type: member.item_type,
                cluster: member.cluster,
                group: Arc::from(""),
                title: Arc::clone(&member.title),
                url: Arc::clone(&member.url),
                epg_channel_id: member.epg_channel_id.clone(),
                upstream_user_agent: member.upstream_user_agent.clone(),
                ..StreamChannel::default()
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod playback_provider_tests {
    use super::*;
    use crate::model::ConfigProvider;
    use shared::model::{ConfigProviderDto, InputType, ProviderUrlSelectionPolicy};

    /// M3U input whose playlist URL is plain HTTP and whose `cdn` profile serves playback only.
    fn independent_playlist_input() -> ConfigInput {
        let provider = Arc::new(ConfigProvider::from(&ConfigProviderDto {
            name: "cdn".into(),
            urls: vec!["http://pl.example.com".into(), "http://eu.example.com".into()],
            provider_url_selection_policy: ProviderUrlSelectionPolicy::RestartFromFirst,
            dns: None,
        }));
        ConfigInput {
            name: "Provider".into(),
            input_type: InputType::M3u,
            url: "http://playlist.example.com/list.m3u?access_key=fixture".into(),
            playback_provider: Some("cdn".into()),
            provider_configs: Some(vec![provider]),
            ..ConfigInput::default()
        }
    }

    /// A `provider://cdn` manifest binds the playback profile although the playlist download stays unbound.
    #[test]
    fn standard_hls_binds_explicit_profile_for_token_only_mapped_manifest() {
        let input = independent_playlist_input();
        assert!(InputSource::from(&input).provider.is_none(), "playlist download must remain independent");
        let url = "provider://cdn/ChannelA/mono.m3u8?token=fixture%2Btoken&keep=1";
        let source = InputSource::for_request_url(&input, url.into());
        assert_eq!(source.url, url);
        let profile = source.provider.expect("mapped standard-HLS URL must retain its registered CDN profile");
        assert_eq!(profile.name.as_ref(), "cdn");
        assert_eq!(profile.urls.len(), 2);
    }

    /// A plain HTTP manifest URL gets no profile.
    #[test]
    fn standard_hls_keeps_plain_http_manifest_unbound() {
        let input = independent_playlist_input();
        let url = "http://pl.example.com/ChannelA/mono.m3u8?token=fixture%2Btoken";
        let source = InputSource::for_request_url(&input, url.into());
        assert_eq!(source.url, url);
        assert!(source.provider.is_none());
    }

    /// An unknown profile name does not fall back to the playback profile.
    #[test]
    fn standard_hls_unknown_profile_does_not_borrow_another_profile() {
        let input = independent_playlist_input();
        let source = InputSource::for_request_url(&input, "provider://unknown/ChannelA/mono.m3u8".into());
        assert!(source.provider.is_none());
    }

    /// An unknown profile name does not fall back to the playlist URL's profile.
    #[test]
    fn standard_hls_unknown_profile_does_not_reuse_the_playlist_profile() {
        let mut input = independent_playlist_input();
        input.url = "provider://cdn/playlist.m3u?access_key=fixture".into();
        assert!(InputSource::from(&input).provider.is_some());
        let source = InputSource::for_request_url(&input, "provider://unknown/ChannelA/mono.m3u8".into());
        assert!(source.provider.is_none(), "an unknown mapped profile must fail closed");
    }

    /// A fresh Stalker link drops the old host's session cookie and keeps unrelated headers.
    #[test]
    fn fresh_link_rebinds_the_session_cookie() {
        use axum::http::{header, HeaderValue};
        let stale = || {
            let mut headers = HeaderMap::new();
            headers.insert(header::COOKIE, HeaderValue::from_static("old-host=1"));
            headers.insert(header::USER_AGENT, HeaderValue::from_static("player"));
            headers
        };

        // No session cookie for the new host and no request cookie before: no cookie is sent.
        let mut headers = stale();
        rebind_session_cookie(&mut headers, None, None);
        assert!(headers.get(header::COOKIE).is_none());
        assert_eq!(headers.get(header::USER_AGENT), Some(&HeaderValue::from_static("player")));

        // A configured request cookie comes back.
        let mut headers = stale();
        rebind_session_cookie(&mut headers, Some(HeaderValue::from_static("configured=1")), None);
        assert_eq!(headers.get(header::COOKIE), Some(&HeaderValue::from_static("configured=1")));

        // The session cookie of the new host is applied.
        let mut headers = stale();
        let fresh = std::collections::HashMap::from([(header::COOKIE.as_str().to_string(), "new-host=2".to_string())]);
        rebind_session_cookie(&mut headers, None, Some(&fresh));
        assert_eq!(headers.get(header::COOKIE), Some(&HeaderValue::from_static("new-host=2")));
    }

    /// Only link rejections (most 4xx, non-playlist body) trigger a fresh `create_link`.
    #[test]
    fn pinned_stalker_playlist_retry_triggers_only_on_rejection() {
        let manifest = |kind| Ok((String::new(), String::new(), HeaderMap::new(), kind));
        let http_err =
            |status| std::io::Error::other(request::request_failed_message(status, "http://example.com/live.m3u8"));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::FORBIDDEN))));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::NOT_FOUND))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::INTERNAL_SERVER_ERROR))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::SERVICE_UNAVAILABLE))));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::UNAUTHORIZED))));
        assert!(is_rejected_hls_manifest_download(&Err(http_err(StatusCode::GONE))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::TOO_MANY_REQUESTS))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::REQUEST_TIMEOUT))));
        assert!(!is_rejected_hls_manifest_download(&Err(http_err(StatusCode::PROXY_AUTHENTICATION_REQUIRED))));
        assert!(!is_rejected_hls_manifest_download(&Err(std::io::Error::from(std::io::ErrorKind::TimedOut))));
        assert!(!is_rejected_hls_manifest_download(&Err(std::io::Error::other("connection reset by peer"))));
        assert!(is_rejected_hls_manifest_download(&manifest(HlsPlaylistKind::Invalid)));
        assert!(!is_rejected_hls_manifest_download(&manifest(HlsPlaylistKind::Media)));
        assert!(!is_rejected_hls_manifest_download(&manifest(HlsPlaylistKind::Master)));
        assert!(super::hls_download_rejects_link(Err(&http_err(StatusCode::FORBIDDEN))));
        assert!(super::hls_download_rejects_link(Ok(HlsPlaylistKind::Invalid)));
        assert!(!super::hls_download_rejects_link(Ok(HlsPlaylistKind::Media)));
        assert!(!super::hls_download_rejects_link(Err(&http_err(StatusCode::INTERNAL_SERVER_ERROR))));
    }

    #[tokio::test]
    async fn get_stream_channel_member_preserves_upstream_user_agent() {
        let env = crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv::new(&["solis", "korath"])
            .build()
            .await;
        let member_vid = env.member_vid("solis");
        let target_path =
            crate::repository::get_target_storage_path(&env.app_state.app_config.config.load(), &env.target.name)
                .expect("target storage path");
        let mut group = (*env.group_record).clone();
        group.members[0].upstream_user_agent = Some(Arc::from("SolisCustomAgent/2.1"));
        tuliprox_repository::FailoverGroupStore::write(&target_path, &[group], &[], None)
            .expect("write group with user agent");
        env.app_state.playlists.bump_failover_group_generation(&env.target.name);

        let channel = get_stream_channel(&env.app_state, &env.target, member_vid).await.expect("member channel");
        assert_eq!(channel.upstream_user_agent, Some(Arc::from("SolisCustomAgent/2.1")));
    }

    #[tokio::test]
    async fn handle_hls_stream_request_member_vid_live_entry_uses_prefer() {
        let env = crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv::new(&["solis", "korath"])
            .hls()
            .build()
            .await;
        // Member 0 is solis, member 1 is korath.
        let korath_member_vid = env.member_vid("korath");
        let response = env.get_hls_manifest(korath_member_vid).await;
        assert_eq!(response.status(), StatusCode::OK);
        // prefer: Some(1) selects korath instead of default member 0 (solis)
        assert_eq!(env.origin_hits("solis"), 0);
        assert_eq!(env.origin_hits("korath"), 1);
        assert_eq!(env.bound_member_input("viewer").await, "korath");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn stale_child_request_for_switched_group_session_does_not_404() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use shared::utils::Internable;
        use std::net::SocketAddr;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().m3u("solis").m3u("korath").build().await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("test user");
        let solis_input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("solis input");
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");
        let korath_cfg = env.app_state.active_provider.find_provider_config(&"korath".intern()).expect("korath cfg");
        let addr: SocketAddr = "127.0.0.1:12345".parse().expect("addr");
        let fingerprint = Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let token = "test-switched-session-token";

        env.app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: env.group_vid,
                provider: &solis_input.name,
                stream_url: &format!("http://{}/live/101", env.origins.get("solis").expect("origin").addr),
                addr: &addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        env.app_state
            .active_users
            .bind_member(
                &user.username,
                token,
                tuliprox_session::MemberBinding {
                    group_vid: env.group_vid,
                    member_vid: env.group_record.members[0].member_vid.get(),
                    member_index: 0,
                    input_name: Arc::clone(&solis_input.name),
                    member_title: "Solis".intern(),
                    provider: Arc::clone(&solis_input.name),
                },
            )
            .await;

        // Runtime failover switches the session to korath (member 1):
        env.app_state
            .active_users
            .bind_member(
                &user.username,
                token,
                tuliprox_session::MemberBinding {
                    group_vid: env.group_vid,
                    member_vid: env.group_record.members[1].member_vid.get(),
                    member_index: 1,
                    input_name: Arc::clone(&korath_input.name),
                    member_title: "Korath".intern(),
                    provider: Arc::clone(&korath_cfg.name),
                },
            )
            .await;
        env.app_state
            .active_users
            .update_session_provider_binding(
                &user.username,
                token,
                Arc::clone(&korath_cfg.name),
                Arc::from(format!("http://{}/live/101", env.origins.get("korath").expect("origin").addr)),
            )
            .await;

        let session = env.app_state.active_users.get_and_update_user_session(&user.username, token).await;
        assert!(session.is_some(), "session exists");

        let stream_source = crate::api::endpoints::hls_api::resolve_hls_virtual_source_for_target(
            &env.app_state,
            &env.target,
            env.group_vid,
        )
        .await
        .expect("source");

        // A stale child request from solis arrives for the switched session.
        let solis_child_stage = HlsRequestStage::Variant {
            source: crate::processing::parser::hls::HlsManifestSource::Child,
            origin_provider: Some("solis"),
            origin_member_vid: None,
        };
        let solis_hits_before = env.origin_hits("solis");
        let korath_hits_before = env.origin_hits("korath");

        let response = handle_hls_stream_request(
            &fingerprint,
            &env.app_state,
            &user,
            &env.target,
            session.as_ref(),
            None,
            "http://127.0.0.1/solis/child.m3u8",
            None,
            stream_source.stream_context,
            &korath_input,
            &HeaderMap::new(),
            shared::model::UserConnectionPermission::Allowed,
            None,
            "/iptv/hls/viewer/secret/live/1.m3u8",
            solis_child_stage,
        )
        .await
        .into_response();

        // Must NOT be 404; must succeed with 200 OK.
        assert_eq!(response.status(), StatusCode::OK);
        // Korath's origin must be hit by the re-resolved request.
        assert_eq!(
            env.origin_hits("korath"),
            korath_hits_before + 1,
            "korath origin must be hit by the re-resolved request"
        );
        // Solis's origin must NOT be hit by this request.
        assert_eq!(
            env.origin_hits("solis"),
            solis_hits_before,
            "solis origin must not be hit by the stale child request"
        );

        let korath_hits_after_first = env.origin_hits("korath");
        let solis_hits_after_first = env.origin_hits("solis");

        // Non-group session (user_session = None) with stale child keeps 404.
        let non_group_source = crate::api::endpoints::hls_api::resolve_hls_virtual_source_for_target(
            &env.app_state,
            &env.target,
            env.plain_vid,
        )
        .await
        .expect("non group source");
        let non_group_response = handle_hls_stream_request(
            &fingerprint,
            &env.app_state,
            &user,
            &env.target,
            None,
            None,
            "http://127.0.0.1/solis/child.m3u8",
            None,
            non_group_source.stream_context,
            &korath_input,
            &HeaderMap::new(),
            shared::model::UserConnectionPermission::Allowed,
            None,
            "/iptv/hls/viewer/secret/live/1.m3u8",
            solis_child_stage,
        )
        .await
        .into_response();
        assert_eq!(non_group_response.status(), StatusCode::NOT_FOUND);
        assert_eq!(env.origin_hits("korath"), korath_hits_after_first);
        assert_eq!(env.origin_hits("solis"), solis_hits_after_first);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn stale_child_of_another_member_of_the_same_provider_re_resolves() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use shared::utils::Internable;
        use std::net::SocketAddr;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().m3u("solis").m3u("korath").build().await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("test user");
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");
        let korath_cfg = env.app_state.active_provider.find_provider_config(&"korath".intern()).expect("korath cfg");
        let korath_origin = env.origins.get("korath").expect("origin").addr;
        let solis_member_vid = env.member_vid("solis");
        let korath_member_vid = env.member_vid("korath");
        let addr: SocketAddr = "127.0.0.1:12345".parse().expect("addr");
        let fingerprint = Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let token = "test-same-provider-member-session";

        env.app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: env.group_vid,
                provider: &korath_cfg.name,
                stream_url: &format!("http://{korath_origin}/live/101"),
                addr: &addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        env.app_state
            .active_users
            .bind_member(
                &user.username,
                token,
                tuliprox_session::MemberBinding {
                    group_vid: env.group_vid,
                    member_vid: korath_member_vid,
                    member_index: 1,
                    input_name: Arc::clone(&korath_input.name),
                    member_title: "Korath".intern(),
                    provider: Arc::clone(&korath_cfg.name),
                },
            )
            .await;
        let session = env.app_state.active_users.get_and_update_user_session(&user.username, token).await;
        assert!(session.is_some(), "session exists");
        let stream_source = crate::api::endpoints::hls_api::resolve_hls_virtual_source_for_target(
            &env.app_state,
            &env.target,
            env.group_vid,
        )
        .await
        .expect("source");
        // Both members resolve through the same provider account; only the member id tells them apart.
        let child_stage = |member_vid| HlsRequestStage::Variant {
            source: crate::processing::parser::hls::HlsManifestSource::Child,
            origin_provider: Some(korath_cfg.name.as_ref()),
            origin_member_vid: Some(member_vid),
        };
        let sealed_child_url = format!("http://{korath_origin}/sealed/child.m3u8");
        let empty_headers = HeaderMap::new();
        // Expands at the call site so each request borrows the session snapshot it was given.
        macro_rules! request {
            ($session:expr, $stage:expr) => {
                handle_hls_stream_request(
                    &fingerprint,
                    &env.app_state,
                    &user,
                    &env.target,
                    $session,
                    None,
                    &sealed_child_url,
                    None,
                    stream_source.stream_context.clone(),
                    &korath_input,
                    &empty_headers,
                    shared::model::UserConnectionPermission::Allowed,
                    None,
                    "/iptv/hls/viewer/secret/live/1.m3u8",
                    $stage,
                )
                .await
                .into_response()
            };
        }
        let sealed_child_hits = || {
            let origin = env.origins.get("korath").expect("korath origin");
            origin
                .requests
                .lock()
                .expect("mutex")
                .iter()
                .filter(|request| request.contains("/sealed/child.m3u8"))
                .count()
        };

        // The child of the bound member is accepted as sealed and fetched as-is.
        let bound_child = request!(session.as_ref(), child_stage(korath_member_vid));
        assert_eq!(bound_child.status(), StatusCode::OK);
        assert_eq!(sealed_child_hits(), 1, "the bound member's child URL is fetched as sealed");

        // The child of the previous member of the same provider is stale and must re-resolve instead.
        let korath_hits_before = env.origin_hits("korath");
        let solis_hits_before = env.origin_hits("solis");
        // A later poll reads the session again, as every HTTP request does. Reusing the snapshot of
        // the previous poll would make the timeline plan stale (an older commit generation).
        let session = env.app_state.active_users.get_and_update_user_session(&user.username, token).await;
        let stale_child = request!(session.as_ref(), child_stage(solis_member_vid));
        assert_eq!(stale_child.status(), StatusCode::OK);
        assert_eq!(sealed_child_hits(), 1, "the stale child URL of the old member must not be fetched");
        assert_eq!(env.origin_hits("korath"), korath_hits_before + 1, "the bound member's own playlist is fetched");
        assert_eq!(env.origin_hits("solis"), solis_hits_before, "the stale member's origin must not be hit");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn stale_child_request_for_switched_stalker_group_session_re_resolves() {
        use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
        use shared::utils::Internable;
        use std::net::SocketAddr;

        let env = GroupPlaybackEnv::builder(&["solis", "korath"])
            .hls()
            .m3u("solis")
            .stalker("korath")
            .stalker_retry_origin("korath", "/live/korath.m3u8")
            .build()
            .await;
        let user = env.app_state.app_config.get_user_credentials("viewer").expect("test user");
        let solis_input = env.app_state.app_config.get_input_by_name(&"solis".intern()).expect("solis input");
        let korath_input = env.app_state.app_config.get_input_by_name(&"korath".intern()).expect("korath input");
        let korath_cfg = env.app_state.active_provider.find_provider_config(&"korath".intern()).expect("korath cfg");
        crate::api::endpoints::hls_api::tests::failover_group::publish_stalker_live_item(&env, &korath_input).await;

        let addr: SocketAddr = "127.0.0.1:12345".parse().expect("addr");
        let fingerprint = Fingerprint::new(format!("fp-{addr}"), addr.ip().to_string(), addr);
        let token = "test-stalker-switched-session-token";

        env.app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: token,
                virtual_id: env.group_vid,
                provider: &solis_input.name,
                stream_url: &format!("http://{}/live/101", env.origins.get("solis").expect("origin").addr),
                addr: &addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        env.app_state
            .active_users
            .bind_member(
                &user.username,
                token,
                tuliprox_session::MemberBinding {
                    group_vid: env.group_vid,
                    member_vid: env.group_record.members[0].member_vid.get(),
                    member_index: 0,
                    input_name: Arc::clone(&solis_input.name),
                    member_title: "Solis".intern(),
                    provider: Arc::clone(&solis_input.name),
                },
            )
            .await;

        // Runtime failover switches the session to korath (Stalker member 1):
        env.app_state
            .active_users
            .bind_member(
                &user.username,
                token,
                tuliprox_session::MemberBinding {
                    group_vid: env.group_vid,
                    member_vid: env.group_record.members[1].member_vid.get(),
                    member_index: 1,
                    input_name: Arc::clone(&korath_input.name),
                    member_title: "Korath".intern(),
                    provider: Arc::clone(&korath_cfg.name),
                },
            )
            .await;
        env.app_state
            .active_users
            .update_session_provider_binding(
                &user.username,
                token,
                Arc::clone(&korath_cfg.name),
                Arc::from(format!("http://{}/live/101", env.origins.get("korath").expect("origin").addr)),
            )
            .await;

        let session = env.app_state.active_users.get_and_update_user_session(&user.username, token).await;
        assert!(session.is_some(), "session exists");

        let stream_source = crate::api::endpoints::hls_api::resolve_hls_virtual_source_for_target(
            &env.app_state,
            &env.target,
            env.group_vid,
        )
        .await
        .expect("source");

        // A stale child request from solis arrives for the switched session.
        let solis_child_stage = HlsRequestStage::Variant {
            source: crate::processing::parser::hls::HlsManifestSource::Child,
            origin_provider: Some("solis"),
            origin_member_vid: None,
        };
        let solis_hits_before = env.origin_hits("solis");
        let korath_hits_before = env.origin_hits("korath");

        let response = handle_hls_stream_request(
            &fingerprint,
            &env.app_state,
            &user,
            &env.target,
            session.as_ref(),
            None,
            "http://127.0.0.1/solis/child.m3u8",
            None,
            stream_source.stream_context,
            &korath_input,
            &HeaderMap::new(),
            shared::model::UserConnectionPermission::Allowed,
            None,
            "/iptv/hls/viewer/secret/live/1.m3u8",
            solis_child_stage,
        )
        .await
        .into_response();

        // Must NOT be 404; must succeed with 200 OK.
        assert_eq!(response.status(), StatusCode::OK);
        // Korath's origin must be hit by the re-resolved request.
        assert!(env.origin_hits("korath") > korath_hits_before, "korath origin must be hit by the re-resolved request");
        // Solis's origin must NOT be hit by this request.
        assert_eq!(
            env.origin_hits("solis"),
            solis_hits_before,
            "solis origin must not be hit by the stale child request"
        );

        let korath_hits_after_first = env.origin_hits("korath");
        let solis_hits_after_first = env.origin_hits("solis");

        // Non-group session (member_binding = None) with stale child keeps 404.
        let non_group_token = "test-stalker-non-group-token";
        env.app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user: &user,
                session_token: non_group_token,
                virtual_id: env.plain_vid,
                provider: &korath_cfg.name,
                stream_url: "http://127.0.0.1/korath/live.m3u8",
                addr: &addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let non_group_session =
            env.app_state.active_users.get_and_update_user_session(&user.username, non_group_token).await;
        assert!(non_group_session.is_some());

        let non_group_source = crate::api::endpoints::hls_api::resolve_hls_virtual_source_for_target(
            &env.app_state,
            &env.target,
            env.plain_vid,
        )
        .await
        .expect("non group source");
        let non_group_response = handle_hls_stream_request(
            &fingerprint,
            &env.app_state,
            &user,
            &env.target,
            non_group_session.as_ref(),
            None,
            "http://127.0.0.1/solis/child.m3u8",
            None,
            non_group_source.stream_context,
            &korath_input,
            &HeaderMap::new(),
            shared::model::UserConnectionPermission::Allowed,
            None,
            "/iptv/hls/viewer/secret/live/1.m3u8",
            solis_child_stage,
        )
        .await
        .into_response();
        assert_eq!(non_group_response.status(), StatusCode::NOT_FOUND);
        assert_eq!(env.origin_hits("korath"), korath_hits_after_first);
        assert_eq!(env.origin_hits("solis"), solis_hits_after_first);
    }
}

#[cfg(test)]
mod legacy_rebase_tests {
    use super::*;

    /// The upstream window the callers parse from the downloaded playlist before the lock.
    fn window(content: &str) -> LegacyMediaWindow { LegacyMediaWindow::parse(content, HlsPlaylistKind::Media) }

    fn plan_media(state: &mut LegacyHlsRebase, content: &str) -> Option<HlsMediaSequenceRebase> {
        // The request snapshot equals the stored state: no other request committed in between.
        let snapshot = *state;
        match plan_legacy_rebase(state, window(content), HlsPlaylistKind::Media, Some(&snapshot)) {
            LegacyRebasePlan::Serve(props) => props,
            LegacyRebasePlan::Stale => panic!("an observed generation cannot be stale"),
        }
    }

    fn plan_master(state: &mut LegacyHlsRebase, content: &str) -> Option<HlsMediaSequenceRebase> {
        let snapshot = *state;
        let window = LegacyMediaWindow::parse(content, HlsPlaylistKind::Master);
        match plan_legacy_rebase(state, window, HlsPlaylistKind::Master, Some(&snapshot)) {
            LegacyRebasePlan::Serve(props) => props,
            LegacyRebasePlan::Stale => panic!("a master playlist is never planned as stale"),
        }
    }

    fn media(sequence: u64, segments: u64) -> String {
        let header = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n");
        (sequence..sequence + segments).fold(header, |body, seq| body + format!("#EXTINF:4,\nseg{seq}.ts\n").as_str())
    }

    fn media_with_disc_seq(sequence: u64, disc_seq: u64, segments: u64) -> String {
        let header = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n#EXT-X-DISCONTINUITY-SEQUENCE:{disc_seq}\n");
        (sequence..sequence + segments).fold(header, |body, seq| body + format!("#EXTINF:4,\nseg{seq}.ts\n").as_str())
    }

    #[test]
    fn media_window_is_parsed_before_the_lock_and_only_for_media_playlists() {
        assert_eq!(
            window(&media_with_disc_seq(104, 7, 3)),
            LegacyMediaWindow { media_sequence: 104, segment_count: 3, discontinuity_sequence: 7 }
        );
        // An empty media playlist has no window; the missing tags read as their RFC defaults.
        assert_eq!(window(""), LegacyMediaWindow { media_sequence: 0, segment_count: 0, discontinuity_sequence: 0 });
        // A master playlist carries no media window and is never scanned.
        assert_eq!(LegacyMediaWindow::parse(&media(5, 4), HlsPlaylistKind::Master), LegacyMediaWindow::default());
    }

    #[test]
    fn discontinuity_boundary_stays_in_every_response_while_in_window() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media(100, 3));

        let rebase1 = plan_media(&mut state, &media(0, 3)).expect("rebase");
        assert_eq!(rebase1.media_sequence, 103);
        assert!(rebase1.insert_discontinuity);

        let rebase2 = plan_media(&mut state, &media(0, 3)).expect("rebase");
        assert_eq!(rebase2.media_sequence, 103);
        assert!(rebase2.insert_discontinuity, "boundary must stay emitted while in the window");

        let rebase3 = plan_media(&mut state, &media(1, 3)).expect("rebase");
        assert_eq!(rebase3.media_sequence, 104);
        assert!(!rebase3.insert_discontinuity);
        assert_eq!(rebase3.discontinuity_sequence, 1, "discontinuity sequence advances when boundary leaves window");
    }

    #[test]
    fn discontinuity_sequence_never_decreases_when_the_new_upstream_counts_lower() {
        let mut state = LegacyHlsRebase::default();
        assert!(plan_media(&mut state, &media_with_disc_seq(100, 50, 3)).is_none());

        let switched = plan_media(&mut state, &media_with_disc_seq(0, 2, 3)).expect("rebase");
        assert_eq!(switched.discontinuity_sequence, 50, "boundary serve keeps the client counter");
        assert!(switched.insert_discontinuity);

        let left = plan_media(&mut state, &media_with_disc_seq(1, 2, 3)).expect("rebase");
        assert_eq!(left.discontinuity_sequence, 51);

        let advanced = plan_media(&mut state, &media_with_disc_seq(2, 3, 3)).expect("rebase");
        assert_eq!(advanced.discontinuity_sequence, 52, "upstream advance carries over");
    }

    #[test]
    fn second_restart_while_the_first_boundary_is_in_the_window_keeps_both_discontinuities() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media(100, 3));
        let first = plan_media(&mut state, &media(0, 3)).expect("rebase");
        assert_eq!((first.media_sequence, first.discontinuity_sequence), (103, 0));
        assert!(first.insert_discontinuity);

        // The upstream restarts again before the first boundary segment left the window.
        let second = plan_media(&mut state, &media(0, 2)).expect("rebase");
        assert_eq!(second.media_sequence, 106, "timeline continues after the last served segment");
        assert!(second.insert_discontinuity, "second boundary is emitted");
        assert_eq!(second.discontinuity_sequence, 1, "first boundary left the window");

        let third = plan_media(&mut state, &media(1, 2)).expect("rebase");
        assert_eq!(third.media_sequence, 107);
        assert!(!third.insert_discontinuity);
        assert_eq!(third.discontinuity_sequence, 2);
    }

    #[test]
    fn discontinuity_sequence_preserves_upstream_advances_and_synthetic_offset() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media_with_disc_seq(100, 2, 3));

        let rebase1 = plan_media(&mut state, &media_with_disc_seq(0, 2, 3)).expect("rebase");
        assert_eq!(rebase1.discontinuity_sequence, 2);
        assert!(rebase1.insert_discontinuity);

        let rebase2 = plan_media(&mut state, &media_with_disc_seq(1, 2, 3)).expect("rebase");
        assert_eq!(rebase2.discontinuity_sequence, 3, "upstream 2 + synthetic 1 = 3");
        assert!(!rebase2.insert_discontinuity);

        let rebase3 = plan_media(&mut state, &media_with_disc_seq(2, 3, 3)).expect("rebase");
        assert_eq!(rebase3.discontinuity_sequence, 4, "upstream 3 + synthetic 1 = 4");
    }

    #[test]
    fn upstream_restart_continues_after_the_last_served_segment_with_a_discontinuity() {
        let mut state = LegacyHlsRebase::default();
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        assert!(plan_media(&mut state, &media(101, 3)).is_none());
        assert_eq!(state.last_served_sequence, 103);

        let rebase = plan_media(&mut state, &media(0, 3)).expect("restart rebases");
        assert_eq!(rebase.media_sequence, 104);
        assert!(rebase.insert_discontinuity);
        assert_eq!(state.last_served_sequence, 106);

        let next = plan_media(&mut state, &media(1, 3)).expect("offset stays");
        assert_eq!(next.media_sequence, 105);
        assert!(!next.insert_discontinuity);
    }

    #[test]
    fn unchanged_window_is_not_a_restart() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media(100, 3));
        // The first serve committed the timeline; an identical window carries no new media.
        assert_eq!((state.last_served_sequence, state.commit_generation), (102, 1));
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        assert_eq!(state.media_sequence_offset, 0);
        assert_eq!(state.commit_generation, 1, "a window that changes nothing must not advance the generation");
    }

    #[test]
    fn master_switch_preserves_client_timeline_on_subsequent_media_serve() {
        let mut state = LegacyHlsRebase::default();
        // Serve A media at seq 10004 with 3 segments (10004, 10005, 10006).
        assert!(plan_media(&mut state, &media(10004, 3)).is_none());
        assert_eq!(state.last_served_sequence, 10006);

        // Simulate master switch: set pending flag as the switch does.
        state.pending_member_rebase = true;
        assert!(plan_master(&mut state, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000\nvariant.m3u8\n").is_none());
        assert!(state.pending_member_rebase);

        // Serve B media at seq 50006 with 3 segments.
        let rebase = plan_media(&mut state, &media(50006, 3)).expect("rebase");
        assert_eq!(rebase.media_sequence, 10007, "served media_sequence == previous last_served + 1");
        assert!(rebase.insert_discontinuity, "insert_discontinuity true");
        assert_eq!(rebase.discontinuity_sequence, 0);
        assert!(!state.pending_member_rebase, "flag cleared after media serve");

        // Same window served again: boundary stays in window.
        let same = plan_media(&mut state, &media(50006, 3)).expect("rebase");
        assert_eq!(same.media_sequence, 10007);
        assert!(same.insert_discontinuity);

        // Window advances so boundary segment (10007) leaves: discontinuity sequence increments.
        let advanced = plan_media(&mut state, &media(50007, 3)).expect("rebase");
        assert_eq!(advanced.media_sequence, 10008);
        assert!(!advanced.insert_discontinuity);
        assert_eq!(advanced.discontinuity_sequence, 1, "discontinuity sequence increments once boundary leaves window");
    }

    #[test]
    fn consecutive_master_switches_before_media_serve_perform_single_rebase() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media(10004, 3));
        assert_eq!(state.last_served_sequence, 10006);

        // Master switch 1
        state.pending_member_rebase = true;
        assert!(plan_master(&mut state, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000\nvariant1.m3u8\n").is_none());

        // Master switch 2 before any child media serve
        state.pending_member_rebase = true;
        assert!(plan_master(&mut state, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000\nvariant2.m3u8\n").is_none());
        assert!(state.pending_member_rebase);

        // Child media serve from final member
        let rebase = plan_media(&mut state, &media(50006, 3)).expect("rebase");
        assert_eq!(rebase.media_sequence, 10007);
        assert!(rebase.insert_discontinuity);
        assert!(!state.pending_member_rebase, "flag cleared after rebase");

        // Subsequent media serve without member switch retains offset and does not rebase again
        let next = plan_media(&mut state, &media(50007, 3)).expect("rebase");
        assert_eq!(next.media_sequence, 10008);
        assert!(!next.insert_discontinuity);
        assert_eq!(next.discontinuity_sequence, 1);
        assert!(!state.pending_member_rebase);
    }

    #[test]
    fn empty_media_playlist_does_not_consume_pending_member_rebase() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media(10004, 3));
        assert_eq!(state.last_served_sequence, 10006);

        // Member switch flag set, then an empty media playlist
        state.pending_member_rebase = true;
        plan_media(&mut state, &media(50000, 0));

        // Then a non-empty one with a higher upstream sequence
        let rebase = plan_media(&mut state, &media(50006, 3)).expect("rebase");
        assert_eq!(
            rebase.media_sequence, 10007,
            "first segment of non-empty window continues at last_served_sequence + 1"
        );
        assert!(rebase.insert_discontinuity, "insert_discontinuity must be true");
    }

    #[test]
    fn empty_media_playlist_without_pending_switch_keeps_last_advertised_window_start() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media(100, 5));
        assert_eq!(state.last_served_start, 100);
        assert_eq!(state.last_served_sequence, 104);

        let empty = plan_media(&mut state, &media(500, 0)).expect("rebase");
        assert_eq!(empty.media_sequence, 100, "must keep last advertised window start");
        assert!(!empty.insert_discontinuity);
    }

    #[test]
    fn empty_media_playlist_does_not_clobber_the_upstream_discontinuity_sequence() {
        let mut state = LegacyHlsRebase::default();
        assert!(plan_media(&mut state, &media_with_disc_seq(100, 3, 3)).is_none());
        assert_eq!(state.upstream_discontinuity_sequence, Some(3));
        assert_eq!(state.discontinuity_sequence, 3);
        assert_eq!(state.last_served_sequence, 102);

        // A momentarily empty live window carries no EXT-X-DISCONTINUITY-SEQUENCE tag at all.
        let empty = plan_media(&mut state, &media(500, 0));
        assert_eq!(
            empty,
            Some(HlsMediaSequenceRebase {
                media_sequence: 100,
                insert_discontinuity: false,
                discontinuity_sequence: 3,
            }),
            "an empty window keeps the client timeline of the last served window"
        );
        assert_eq!(
            state.upstream_discontinuity_sequence,
            Some(3),
            "an empty window must not reset the upstream discontinuity counter"
        );
        assert_eq!(state.discontinuity_sequence, 3);

        // The next real window advances the upstream counter 3 -> 4: the client counter follows by
        // one, not by the whole upstream value.
        let next = plan_media(&mut state, &media_with_disc_seq(103, 4, 3));
        assert_eq!(state.discontinuity_sequence, 4, "only the upstream advance is added");
        assert!(next.is_none(), "a plain advance within the same upstream needs no rebase");
    }

    #[test]
    fn empty_media_playlist_with_pending_member_rebase_does_not_change_discontinuity_counter() {
        let mut state = LegacyHlsRebase::default();
        plan_media(&mut state, &media_with_disc_seq(10004, 3, 3));
        assert_eq!(state.last_served_sequence, 10006);
        assert_eq!(state.discontinuity_sequence, 3);
        assert_eq!(state.upstream_discontinuity_sequence, Some(3));

        state.pending_member_rebase = true;
        let empty_rebase = plan_media(&mut state, &media_with_disc_seq(50000, 40, 0));
        assert_eq!(
            empty_rebase,
            Some(HlsMediaSequenceRebase {
                media_sequence: 10007,
                insert_discontinuity: false,
                discontinuity_sequence: 3,
            })
        );
        assert_eq!(state.discontinuity_sequence, 3);
        assert_eq!(state.upstream_discontinuity_sequence, Some(3));
        assert!(state.pending_member_rebase);

        let rebase = plan_media(&mut state, &media_with_disc_seq(50006, 40, 3)).expect("rebase");
        assert_eq!(rebase.media_sequence, 10007);
        assert!(rebase.insert_discontinuity);
        assert_eq!(rebase.discontinuity_sequence, 3);
    }

    #[test]
    fn stale_generation_keeps_the_committed_timeline_but_a_real_restart_rebases() {
        let mut state = LegacyHlsRebase::default();
        // Commit the base window (100..102) as generation 1.
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        assert_eq!(state.last_served_sequence, 102);
        assert_eq!(state.commit_generation, 1);
        let pre_restart = state;

        // A genuine upstream restart commits 103..105 as generation 2.
        let restart = plan_media(&mut state, &media(0, 3)).expect("restart rebases");
        assert_eq!(restart.media_sequence, 103);
        assert!(restart.insert_discontinuity);
        assert_eq!(state.last_served_sequence, 105);
        assert_eq!(state.commit_generation, 2);
        let committed = state;

        // A request that read generation 1 with offset 0 before downloading now answers with its
        // older 0..1 window. Serving it would re-tag media the client already played and drop the
        // restart boundary, so the plan is stale and the committed timeline stays untouched.
        assert_eq!(
            plan_legacy_rebase(&mut state, window(&media(0, 2)), HlsPlaylistKind::Media, Some(&pre_restart)),
            LegacyRebasePlan::Stale
        );
        assert_eq!(state, committed, "a stale response must not change the committed timeline");

        // A pre-restart snapshot stays stale even when its window reaches the committed tail: the
        // restart commit replaced the offset that window was fetched and ranked against.
        assert_eq!(
            plan_legacy_rebase(&mut state, window(&media(0, 3)), HlsPlaylistKind::Media, Some(&pre_restart)),
            LegacyRebasePlan::Stale
        );
        assert_eq!(state, committed);

        // The same older window with the observed generation unchanged is a genuine restart.
        let observed = state;
        let rebase = plan_legacy_rebase(&mut state, window(&media(0, 2)), HlsPlaylistKind::Media, Some(&observed));
        assert_eq!(
            rebase,
            LegacyRebasePlan::Serve(Some(HlsMediaSequenceRebase {
                media_sequence: 106,
                insert_discontinuity: true,
                discontinuity_sequence: 1,
            }))
        );
        assert_eq!(state.last_served_sequence, 107);
        assert_eq!(state.commit_generation, 3);
    }

    #[test]
    fn stale_generation_with_an_unchanged_mapping_serves_a_window_reaching_the_committed_tail() {
        let mut state = LegacyHlsRebase::default();
        // Commit 100..102 as generation 1.
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        let first = state;
        // A plain window advance commits generation 2 without touching the offset.
        assert!(plan_media(&mut state, &media(103, 3)).is_none(), "a plain window advance needs no rebase");
        assert_eq!((state.media_sequence_offset, state.last_served_sequence, state.commit_generation), (0, 105, 2));

        // A stale snapshot (generation 1, the offset this request was mapped with) answers with a
        // window that carries media past the committed tail: it is new media, so it is served.
        let plan = plan_legacy_rebase(&mut state, window(&media(104, 3)), HlsPlaylistKind::Media, Some(&first));
        assert_eq!(plan, LegacyRebasePlan::Serve(None));
        assert_eq!(state.last_served_sequence, 106);
        assert_eq!(state.commit_generation, 3);
    }

    #[test]
    fn pre_restart_window_from_a_stale_snapshot_is_stale_even_ahead_of_the_committed_tail() {
        let mut state = LegacyHlsRebase::default();
        // Base serve: offset 0, 100..102 committed as generation 1.
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        assert_eq!((state.media_sequence_offset, state.last_served_sequence, state.commit_generation), (0, 102, 1));

        // R1 and R2 snap the same session state before downloading.
        let snapshot = state;

        // R1 downloads the restarted window 0..2 and commits offset 103, last served 105.
        let restart = plan_legacy_rebase(&mut state, window(&media(0, 3)), HlsPlaylistKind::Media, Some(&snapshot));
        assert_eq!(
            restart,
            LegacyRebasePlan::Serve(Some(HlsMediaSequenceRebase {
                media_sequence: 103,
                insert_discontinuity: true,
                discontinuity_sequence: 0,
            }))
        );
        assert_eq!((state.media_sequence_offset, state.last_served_sequence, state.commit_generation), (103, 105, 2));
        let committed = state;

        // R2 answers with its pre-restart window 100..102. Ranked with the committed offset it maps
        // to 203..205, ahead of the committed tail, so the window bound alone would serve it and
        // consume the restart boundary. The offset recorded with the snapshot marks it stale.
        let mapped_ahead = 100_u64.saturating_add_signed(committed.media_sequence_offset);
        assert_eq!(mapped_ahead, 203);
        assert!(mapped_ahead + 2 > committed.last_served_sequence, "the window bound alone would look current");
        assert_eq!(
            plan_legacy_rebase(&mut state, window(&media(100, 3)), HlsPlaylistKind::Media, Some(&snapshot)),
            LegacyRebasePlan::Stale
        );
        assert_eq!(state, committed, "a stale pre-restart window must not consume the boundary");

        // The next live poll of the committed window is served with the boundary intact and does
        // not restart again.
        let live_snapshot = state;
        let live = plan_legacy_rebase(&mut state, window(&media(0, 3)), HlsPlaylistKind::Media, Some(&live_snapshot));
        assert_eq!(
            live,
            LegacyRebasePlan::Serve(Some(HlsMediaSequenceRebase {
                media_sequence: 103,
                insert_discontinuity: true,
                discontinuity_sequence: 0,
            }))
        );
        assert_eq!(state, committed, "a live poll of the committed window is not a restart");
    }

    #[test]
    fn stale_snapshot_serves_an_identical_reserve_of_the_committed_window() {
        let mut state = LegacyHlsRebase::default();
        // Commit 100..102 as generation 1.
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        let snapshot = state;

        // R1 commits the next window 103..105 as generation 2; the mapping did not change.
        assert!(plan_media(&mut state, &media(103, 3)).is_none(), "a plain window advance needs no rebase");
        assert_eq!(
            (state.media_sequence_offset, state.last_served_start, state.last_served_sequence, state.commit_generation),
            (0, 103, 105, 2)
        );

        // R2 fetched the same 103..105 from the older snapshot. Serving it re-tags the committed
        // window identically, so it is the harmless duplicate re-serve, not a stale response.
        let plan = plan_legacy_rebase(&mut state, window(&media(103, 3)), HlsPlaylistKind::Media, Some(&snapshot));
        assert_eq!(plan, LegacyRebasePlan::Serve(None));
        assert_eq!(
            (state.last_served_start, state.last_served_sequence, state.commit_generation),
            (103, 105, 2),
            "a duplicate serve changes no timeline"
        );
    }

    #[test]
    fn stale_generation_window_ending_at_the_committed_tail_is_stale() {
        let mut state = LegacyHlsRebase::default();
        // Commit 100..102 as generation 1 without a rebase (offset 0).
        assert!(plan_media(&mut state, &media(100, 3)).is_none());
        assert_eq!((state.media_sequence_offset, state.last_served_sequence, state.commit_generation), (0, 102, 1));

        // R1 and R2 snapshot that state before downloading.
        let snapshot = state;

        // R1 commits the next window 103..105 as generation 2; the mapping did not change.
        assert!(plan_media(&mut state, &media(103, 3)).is_none(), "a plain window advance needs no rebase");
        assert_eq!((state.media_sequence_offset, state.last_served_sequence, state.commit_generation), (0, 105, 2));
        let committed = state;

        // R2 answers from the old snapshot with 98..105: its tail is exactly the committed tail, so
        // it carries no new media and would serve a smaller media sequence than the client already
        // saw. The window bound alone (`<`) accepts it; it must be stale.
        assert_eq!(
            plan_legacy_rebase(&mut state, window(&media(98, 8)), HlsPlaylistKind::Media, Some(&snapshot)),
            LegacyRebasePlan::Stale
        );
        assert_eq!(state, committed, "a stale response must not rewind the committed timeline");
    }

    #[test]
    fn stale_generation_window_with_the_same_start_but_a_shorter_tail_is_stale() {
        let mut state = LegacyHlsRebase::default();
        // Commit 100..104 as generation 1; R2 snapshots that state before downloading.
        assert!(plan_media(&mut state, &media(100, 5)).is_none());
        assert_eq!((state.last_served_start, state.last_served_sequence, state.commit_generation), (100, 104, 1));
        let snapshot = state;

        // R1 commits the origin's grown window 100..109 as generation 2: same start, longer tail.
        assert!(plan_media(&mut state, &media(100, 10)).is_none(), "an in-place window growth needs no rebase");
        assert_eq!((state.last_served_start, state.last_served_sequence, state.commit_generation), (100, 109, 2));
        let committed = state;

        // R2 answers from its older snapshot with 100..104: same start, shorter tail. Its mapped end
        // is before the committed tail, so it carries no new media; serving it would re-tag already
        // served media and move the committed tail back to 104.
        assert_eq!(
            plan_legacy_rebase(&mut state, window(&media(100, 5)), HlsPlaylistKind::Media, Some(&snapshot)),
            LegacyRebasePlan::Stale
        );
        assert_eq!(state, committed, "a stale shorter window must not move the committed tail");

        // The identical re-serve of the committed window from the same older snapshot is served and
        // changes nothing: its mapped end equals the committed tail.
        assert_eq!(
            plan_legacy_rebase(&mut state, window(&media(100, 10)), HlsPlaylistKind::Media, Some(&snapshot)),
            LegacyRebasePlan::Serve(None)
        );
        assert_eq!(state, committed, "an identical re-serve changes no timeline");
    }
}
