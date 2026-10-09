use super::{get_stream_config_u64, mark_response_as_uncompressed, try_unwrap_body};
use crate::{
    api::{
        endpoints::hls_api::{hls_media_playlist_wrap_enabled, wrap_media_playlist, HlsMediaPlaylistWrap},
        model::{AppState, BoxedProviderStream, StreamDetails, StreamError},
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, PlaybackKind, ProxyUserCredentials},
    processing::parser::hls::{classify_hls_playlist, rewrite_hls, HlsPlaylistKind, RewriteHlsProps},
};
use axum::{
    body::Body,
    http::{header, StatusCode},
    response::IntoResponse,
};
use bytes::{Bytes, BytesMut};
use futures::{stream, StreamExt};
use shared::{
    concat_string,
    defaults::default_catchup_session_ttl_secs,
    model::{UserConnectionPermission, VirtualId},
};
use std::{sync::Arc, time::Duration};
use tuliprox_hls::api::MAX_HLS_MANIFEST_BYTES;
use url::Url;

pub(super) enum CatchupPayload {
    Direct(BoxedProviderStream),
    HlsManifest(Bytes),
}

pub(super) struct DetectedCatchupHlsResponseParams<'a> {
    pub(super) app_state: &'a Arc<AppState>,
    pub(super) stream_details: StreamDetails,
    pub(super) manifest: Bytes,
    pub(super) user: &'a ProxyUserCredentials,
    pub(super) target: &'a ConfigTarget,
    pub(super) input: &'a ConfigInput,
    pub(super) fingerprint: &'a Fingerprint,
    pub(super) session_token: &'a str,
    pub(super) virtual_id: VirtualId,
    pub(super) connection_permission: UserConnectionPermission,
    pub(super) connection_kind: crate::api::model::ConnectionKind,
    pub(super) fallback_stream_url: &'a str,
}

#[allow(clippy::too_many_lines)]
pub(super) async fn detected_catchup_hls_response(
    params: DetectedCatchupHlsResponseParams<'_>,
) -> axum::response::Response {
    let DetectedCatchupHlsResponseParams {
        app_state,
        mut stream_details,
        manifest,
        user,
        target,
        input,
        fingerprint,
        session_token,
        virtual_id,
        connection_permission,
        connection_kind,
        fallback_stream_url,
    } = params;

    let Some(provider) = stream_details.provider_name.clone() else {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Some(server_info) = app_state.app_config.get_user_server_info(user) else {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok(content) = std::str::from_utf8(&manifest) else {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let playlist_kind = classify_hls_playlist(content);
    if playlist_kind == HlsPlaylistKind::Invalid {
        cleanup_failed_detected_catchup_hls(app_state, &mut stream_details, &user.username, session_token).await;
        return StatusCode::BAD_GATEWAY.into_response();
    }

    let response_url = stream_details
        .stream_info
        .as_ref()
        .and_then(|(_, _, response_url, _)| response_url.as_ref())
        .map_or_else(|| fallback_stream_url.to_string(), ToString::to_string);
    let base_url = server_info.get_base_url();
    let encrypt_secret = app_state.get_encrypt_secret();
    let rewritten = rewrite_hls(
        user,
        &RewriteHlsProps {
            secret: &encrypt_secret,
            base_url: &base_url,
            content,
            hls_url: response_url,
            target_id: target.id,
            virtual_id: virtual_id.get(),
            input_id: input.id,
            user_token: Some(session_token),
            origin_provider: Some(&provider),
            playlist_kind: Some(playlist_kind),
        },
    );

    let request_url = stream_details.request_url.as_deref().unwrap_or(fallback_stream_url);
    let created_session_token = app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id: virtual_id.get(),
            provider: &provider,
            stream_url: request_url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind: Some(connection_kind),
            socket_bound: false,
        })
        .await;
    if let Some(stream_index) = stream_details.user_agent_stream_index {
        app_state
            .active_users
            .set_user_agent_stream_index_if_absent(&user.username, &created_session_token, stream_index)
            .await;
    }
    if !stream_details.provider_session_headers.is_empty() {
        app_state
            .active_users
            .update_session_provider_response_headers_from(
                &user.username,
                &created_session_token,
                &stream_details.provider_session_headers,
                stream_details
                    .stream_info
                    .as_ref()
                    .and_then(|(_, _, url, _)| url.as_ref())
                    .map_or(request_url, Url::as_str),
            )
            .await;
    }
    app_state.active_provider.refresh_adaptive_playback_lease(
        &provider,
        &created_session_token,
        PlaybackKind::Catchup,
        get_catchup_session_ttl_secs(app_state),
    );
    let binding_tag = stream_details.provider_handle.as_ref().and_then(|handle| handle.handle()?.binding_tag);
    app_state.connection_manager.release_managed_provider_handle(stream_details.provider_handle.take());
    app_state
        .active_users
        .release_unbound_session_reservation(&user.username, &created_session_token, None, false)
        .await;
    app_state.active_users.clear_unbound_session_addr(&user.username, &created_session_token, &fingerprint.addr).await;

    if playlist_kind == HlsPlaylistKind::Media && hls_media_playlist_wrap_enabled(app_state, target, user) {
        let master = wrap_media_playlist(
            HlsMediaPlaylistWrap {
                app_state,
                user,
                base_url: &base_url,
                target_id: target.id,
                input,
                virtual_id: virtual_id.get(),
                session_token: &created_session_token,
                // The canonical catch-up URL, not the provider-resolved one, so a refresh can
                // resolve it to whichever account is selected then.
                sealed_url: fallback_stream_url,
                provider: &provider,
                binding_tag,
                known_bitrate_bps: None,
                stream_ref: None,
            },
            rewritten,
        )
        .await;
        return catchup_hls_manifest_response(master);
    }
    catchup_hls_manifest_response(rewritten)
}

pub(super) fn catchup_hls_manifest_response(content: String) -> axum::response::Response {
    let mut response = try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::api::static_headers::CT_M3U.clone())
        .header(header::CACHE_CONTROL, crate::api::static_headers::CC_NO_STORE.clone())
        .body(Body::from(content)));
    mark_response_as_uncompressed(&mut response);
    response
}

pub(super) async fn cleanup_failed_detected_catchup_hls(
    app_state: &Arc<AppState>,
    stream_details: &mut StreamDetails,
    username: &str,
    session_token: &str,
) {
    app_state.connection_manager.release_managed_provider_handle(stream_details.provider_handle.take());
    app_state.active_users.terminate_session(username, session_token).await;
    app_state.active_provider.clear_provider_reservation(session_token);
}

pub(super) async fn probe_catchup_payload(
    stream: BoxedProviderStream,
    deadline: Duration,
) -> Result<CatchupPayload, StreamError> {
    tokio::time::timeout(deadline, probe_catchup_payload_inner(stream))
        .await
        .map_err(|_| StreamError::Stream("catch-up payload probe timed out".to_string()))?
}

pub(super) async fn probe_catchup_payload_inner(
    mut stream: BoxedProviderStream,
) -> Result<CatchupPayload, StreamError> {
    const HLS_SIGNATURE: &[u8] = b"#EXTM3U";

    let mut prefix = BytesMut::new();
    while prefix.len() < HLS_SIGNATURE.len() {
        let Some(chunk) = stream.next().await else {
            return Ok(CatchupPayload::Direct(stream::once(async move { Ok(prefix.freeze()) }).chain(stream).boxed()));
        };
        prefix.extend_from_slice(&chunk?);
    }

    if !prefix.starts_with(HLS_SIGNATURE) {
        return Ok(CatchupPayload::Direct(stream::once(async move { Ok(prefix.freeze()) }).chain(stream).boxed()));
    }
    if prefix.len() > MAX_HLS_MANIFEST_BYTES {
        return Err(StreamError::Stream("catch-up HLS manifest exceeds size limit".to_string()));
    }

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if prefix.len().saturating_add(chunk.len()) > MAX_HLS_MANIFEST_BYTES {
            return Err(StreamError::Stream("catch-up HLS manifest exceeds size limit".to_string()));
        }
        prefix.extend_from_slice(&chunk);
    }

    Ok(CatchupPayload::HlsManifest(prefix.freeze()))
}

pub(crate) fn get_catchup_session_ttl_secs(app_state: &Arc<AppState>) -> u64 {
    get_stream_config_u64(app_state, |stream| stream.catchup_session_ttl_secs, default_catchup_session_ttl_secs())
}

pub fn create_catchup_session_key(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    archive_discriminator: &str,
) -> String {
    concat_string!(
        "catchup|",
        &fingerprint.key,
        "|",
        username,
        "|",
        &virtual_id.to_string(),
        "|",
        archive_discriminator.trim_matches('/')
    )
}

pub fn create_m3u_catchup_session_key(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    archive_discriminator: &str,
) -> String {
    concat_string!(
        "m3u-catchup|",
        &fingerprint.key,
        "|",
        username,
        "|",
        &virtual_id.to_string(),
        "|",
        archive_discriminator
    )
}
