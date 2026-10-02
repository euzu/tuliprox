#![allow(clippy::wildcard_imports)]
use super::*;
use crate::processing::parser::hls::{
    build_hls_resource_uri, create_hls_resource_token, HlsManifestSource, HlsResourceKind, HlsResourceToken,
};
use tuliprox_core::model::ProviderBindingTag;

/// Which request reaches `handle_hls_stream_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::api) enum HlsRequestStage<'a> {
    /// Entry route: the URL is normalized and a media playlist may be wrapped in a master.
    Entry,
    /// Refresh of a sealed manifest token; the URL is used exactly as sealed.
    Variant { source: HlsManifestSource, origin_provider: Option<&'a str> },
    /// Manifest token without an explicit kind (issued before tokens carried one);
    /// behaves like the token route did before.
    LegacyVariant,
}

impl HlsRequestStage<'_> {
    pub(in crate::api) const fn normalizes_url(self) -> bool { !matches!(self, Self::Variant { .. }) }
}

/// Appends the provider session cookie stored on the user session to a manifest request.
pub(in crate::api) fn append_user_session_provider_headers(
    headers: &mut HeaderMap,
    provider_session_headers: &HashMap<String, String>,
) {
    if let Some(cookie) =
        provider_session_headers.get(header::COOKIE.as_str()).and_then(|value| HeaderValue::from_str(value).ok())
    {
        headers.insert(header::COOKIE, cookie);
    }
}

/// True when an entry media playlist should be answered with a single-variant master.
pub(in crate::api) fn hls_media_playlist_wrap_enabled(app_state: &Arc<AppState>, target: &ConfigTarget) -> bool {
    app_state.app_config.config.load().get_hls_wrap_media_playlist() && !hls_cache_enabled_for_target(app_state, target)
}

pub(in crate::api) struct HlsMediaPlaylistWrap<'a> {
    pub(in crate::api) app_state: &'a Arc<AppState>,
    pub(in crate::api) user: &'a ProxyUserCredentials,
    pub(in crate::api) base_url: &'a str,
    pub(in crate::api) target_id: u16,
    pub(in crate::api) input: &'a ConfigInput,
    pub(in crate::api) virtual_id: u32,
    pub(in crate::api) session_token: &'a str,
    /// Pre-redirect upstream request URL; sealed into the variant token and used as hand-off key.
    pub(in crate::api) sealed_url: &'a str,
    pub(in crate::api) provider: &'a Arc<str>,
    pub(in crate::api) binding_tag: Option<ProviderBindingTag>,
    pub(in crate::api) known_bitrate_bps: Option<u32>,
    pub(in crate::api) stream_ref: Option<&'a str>,
}

/// Stores the rewritten media playlist for the immediate variant request and renders the
/// single-variant master whose only URI is a sealed `Manifest(Entry)` token.
pub(in crate::api) async fn wrap_media_playlist(wrap: HlsMediaPlaylistWrap<'_>, rewritten_media: String) -> String {
    let HlsMediaPlaylistWrap {
        app_state,
        user,
        base_url,
        target_id,
        input,
        virtual_id,
        session_token,
        sealed_url,
        provider,
        binding_tag,
        known_bitrate_bps,
        stream_ref,
    } = wrap;
    app_state.hls.playlist_handoff.insert(
        session_token,
        sealed_url,
        rewritten_media,
        Arc::clone(provider),
        binding_tag,
    );

    let encrypt_secret = app_state.get_encrypt_secret();
    let token = create_hls_resource_token(
        &encrypt_secret,
        Some(session_token),
        sealed_url,
        Some(HlsResourceKind::Manifest(HlsManifestSource::Entry)),
        Some(provider),
    );
    let variant_uri = build_hls_resource_uri(base_url, user, target_id, input.id, virtual_id, &token);

    let database_bitrate_bps = match stream_ref {
        Some(stream_ref) if HlsMasterBandwidth::new(known_bitrate_bps).is_unknown() => {
            match load_input_live_bitrate_bps(&app_state.app_config, input, stream_ref).await {
                Ok(bitrate_bps) => bitrate_bps,
                Err(err) => {
                    warn!("HLS entry live bitrate lookup failed; using fallback: input_id={} error={err}", input.id);
                    None
                }
            }
        }
        _ => None,
    };
    let selection = HlsMasterBandwidthSelection::resolve(known_bitrate_bps, database_bitrate_bps);
    HlsSingleVariantMasterPlaylist::new(selection.bandwidth(), variant_uri).render()
}

/// Output scope an entry route checks before content filters.
#[derive(Debug, Clone, Copy)]
pub(in crate::api) enum HlsEntryOutputScope {
    ItemType(PlaylistItemType),
    /// Xtream timeshift plays the live item as archive.
    LiveCluster,
}

/// Content access check of the entry routes: output cluster or item type, then the user's `t_filter`.
pub(in crate::api) fn user_allows_entry_content<T>(
    user: &ProxyUserCredentials,
    scope: HlsEntryOutputScope,
    item: &T,
) -> bool
where
    for<'a> shared::model::PlaylistItem: From<&'a T>,
{
    let output_allowed = match scope {
        HlsEntryOutputScope::ItemType(item_type) => user.allows_item_type(item_type),
        HlsEntryOutputScope::LiveCluster => user.allows_cluster(XtreamCluster::Live),
    };
    output_allowed && (user.t_filter.is_none() || user.allows_content(&shared::model::PlaylistItem::from(item)))
}

/// Checks that a sealed session token belongs to the requesting user, `virtual_id` and user agent.
/// Only the client IP may differ: that is the identity change the sealed token survives.
pub(super) fn hls_session_hint_matches_requester(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    hint: &str,
) -> bool {
    fn without_client_ip(identity: &str) -> Option<&str> { identity.split_once('|').map(|(_, rest)| rest) }

    let current =
        create_playback_session_fingerprint(fingerprint, username, virtual_id, PlaylistItemType::LiveHls, None);
    let Some(current_identity) = without_client_ip(&current) else {
        return false;
    };
    if let Some(catchup) = hint.strip_prefix("m3u-catchup|") {
        return without_client_ip(catchup)
            .and_then(|rest| rest.strip_prefix(current_identity))
            .is_some_and(|archive| archive.starts_with('|'));
    }
    is_live_hls_session_token(hint)
        && hint.rsplit_once("|hls|").and_then(|(base, _)| without_client_ip(base)) == Some(current_identity)
}

/// Live HLS session token `<base>|hls|<random16>`, as created by `hls_entry_user_session_token`.
pub(super) fn is_live_hls_session_token(token: &str) -> bool {
    token.rsplit_once("|hls|").is_some_and(|(base, suffix)| {
        !base.is_empty() && suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

/// Entry-route content access for a recreated session: `Some(false)` denied, `None` unknown item.
async fn hls_recreate_content_allowed(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    target: &Arc<ConfigTarget>,
    virtual_id: u32,
    is_catchup: bool,
) -> Option<bool> {
    if target.has_output(TargetType::Xtream) {
        if let Ok(item) =
            xtream_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target, None).await
        {
            let scope = if is_catchup {
                HlsEntryOutputScope::LiveCluster
            } else {
                HlsEntryOutputScope::ItemType(item.item_type)
            };
            return Some(user_allows_entry_content(user, scope, &item));
        }
    }
    let item =
        m3u_get_item_for_stream_id(virtual_id, &app_state.app_config, &app_state.playlists, target).await.ok()?;
    Some(user_allows_entry_content(user, HlsEntryOutputScope::ItemType(item.item_type), &item))
}

/// Recreates a missing user session from a sealed `Manifest(Entry)` token.
///
/// With the wrapper the player refreshes only the variant URI, so after a session expiry the
/// token route has to do what an entry refresh did before: identity, access, admission, source,
/// session, fetch. The session keeps the sealed session token and therefore its playback owner.
#[allow(clippy::too_many_arguments)]
pub(super) async fn recreate_hls_session_from_token(
    fingerprint: &Fingerprint,
    req_headers: &HeaderMap,
    app_state: &Arc<AppState>,
    user: &Arc<ProxyUserCredentials>,
    target: &Arc<ConfigTarget>,
    input: &Arc<ConfigInput>,
    virtual_id: u32,
    decoded: &HlsResourceToken,
) -> axum::response::Response {
    let (Some(HlsResourceKind::Manifest(HlsManifestSource::Entry)), Some(hint)) =
        (decoded.kind, decoded.session_token.as_deref())
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if hls_cache_enabled_for_target(app_state, target)
        || !hls_session_hint_matches_requester(fingerprint, &user.username, virtual_id, hint)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // Only an expired session comes back; an evicted, kicked or terminated one stays ended,
    // otherwise two devices over the connection limit would keep evicting each other.
    if app_state.active_users.is_session_ended(hint).await {
        debug_if_enabled!("HLS session recreate refused for ended session {}", sanitize_sensitive_info(hint));
        return StatusCode::BAD_REQUEST.into_response();
    }

    match hls_recreate_content_allowed(app_state, user, target, virtual_id, is_m3u_catchup_session_token(hint)).await {
        Some(true) => {}
        allowed => {
            let status = if allowed.is_none() { StatusCode::NOT_FOUND } else { StatusCode::FORBIDDEN };
            return hls_custom_video_manifest_response(
                app_state,
                user,
                CustomVideoStreamType::ChannelUnavailable,
                status,
            )
            .await;
        }
    }

    let archive_reference = resolve_m3u_archive_reference(&decoded.url, Some(hint));
    // Same admission as the entry routes: Activate with strategies and the eviction guard.
    let (connection_admission, _, _) = resolve_playback_request_admission(
        &app_state.admission_ctx(),
        user,
        fingerprint,
        None,
        hint,
        false,
        EvictionReentryGuard::Session(hint),
        false,
        false,
    )
    .await;
    let connection_permission = connection_admission.permission();
    if connection_permission == UserConnectionPermission::Exhausted {
        if connection_admission.is_reentry_suppressed() {
            return crate::api::api_utils::reentry_suppressed_response();
        }
        let stream_channel =
            resolve_stream_channel(app_state, target, input, virtual_id, &decoded.url, archive_reference, Some(hint))
                .await;
        return hls_admission_failure_manifest_response(
            app_state,
            fingerprint,
            user,
            stream_channel,
            input.name.clone(),
            req_headers,
            ConnectFailureReason::UserConnectionsExhausted,
        )
        .await;
    }
    let connection_kind = connection_admission.kind().unwrap_or(crate::api::model::ConnectionKind::Normal);

    let source = match resolve_hls_virtual_source_for_target(app_state, target, virtual_id).await {
        Ok(source) if source.input.id == input.id => source,
        Ok(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(status) => return status.into_response(),
    };
    // The lease, and with it the binding tag, can outlive the user session.
    app_state.hls.playlist_handoff.remove_session(hint);
    let original_hls_entry_path = build_virtual_hls_entry_path(target, input, user, virtual_id);
    handle_hls_stream_request(
        fingerprint,
        app_state,
        user,
        target,
        None,
        Some(hint),
        &decoded.url,
        archive_reference,
        source.stream_context,
        input,
        req_headers,
        connection_permission,
        Some(connection_kind),
        &original_hls_entry_path,
        HlsRequestStage::Variant {
            source: HlsManifestSource::Entry,
            origin_provider: decoded.origin_provider.as_deref(),
        },
    )
    .await
    .into_response()
}
