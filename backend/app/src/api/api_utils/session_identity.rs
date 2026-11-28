use super::{get_catchup_session_ttl_secs, get_stream_config_u64};
use crate::{
    api::model::{AppState, CustomVideoStreamType, StreamDetails, UserSession},
    auth::Fingerprint,
};
use axum::http::HeaderMap;
use shared::{
    concat_string,
    defaults::{default_hls_session_ttl_secs, DASH_EXT, HLS_EXT},
    model::PlaylistItemType,
};
use smallvec::SmallVec;
use std::{net::SocketAddr, sync::Arc};

pub(crate) const HEADER_TULIPROX_PLAYBACK_ID: &str = "X-Tuliprox-Playback-Id";
pub(crate) const HEADER_PLAYBACK_SESSION_ID: &str = "X-Playback-Session-Id";
const MAX_PLAYBACK_ID_LEN: usize = 128;

fn parse_valid_playback_id_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let value = headers.get(name)?;
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_PLAYBACK_ID_LEN || !bytes.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// Extracts a client-generated playback identifier from `X-Tuliprox-Playback-Id`,
/// falling back to Apple's `X-Playback-Session-Id`. Values must be 1-128 visible
/// ASCII characters; anything else is ignored.
pub(crate) fn extract_valid_playback_id(headers: &HeaderMap) -> Option<&str> {
    parse_valid_playback_id_header(headers, HEADER_TULIPROX_PLAYBACK_ID)
        .or_else(|| parse_valid_playback_id_header(headers, HEADER_PLAYBACK_SESSION_ID))
}

/// Returns the effective client-generated playback identifier when the user has
/// Player API access and is not an internal recording proxy user.
pub(crate) fn effective_playback_id_for_user<'a>(
    config: &crate::model::Config,
    user: &crate::model::ProxyUserCredentials,
    headers: &'a HeaderMap,
) -> Option<&'a str> {
    if user.is_recording_proxy_user() || !crate::api::auth_middleware::player_api_reachable(config, user) {
        return None;
    }
    extract_valid_playback_id(headers)
}

pub(crate) fn get_hls_session_ttl_secs(app_state: &Arc<AppState>) -> u64 {
    get_stream_config_u64(app_state, |stream| stream.hls_session_ttl_secs, default_hls_session_ttl_secs())
}

pub(crate) fn get_session_reservation_ttl_secs(app_state: &Arc<AppState>, item_type: PlaylistItemType) -> u64 {
    match item_type {
        PlaylistItemType::LiveHls | PlaylistItemType::LiveDash => get_hls_session_ttl_secs(app_state),
        PlaylistItemType::Catchup => get_catchup_session_ttl_secs(app_state),
        _ => 0,
    }
}

/// Whether the session should pin the provider account via `refresh_provider_reservation`.
///
/// A non-Provisioning custom video (`ChannelUnavailable`, `ProviderConnectionsExhausted`, …) means
/// the upstream open already failed. The provider connection slot was released by
/// `create_provider_stream`, and the custom video is a local fallback served to the client.
/// Pinning the provider via `refresh_provider_reservation` would hold the provider account for
/// the configured session TTL (e.g. `catchup_session_ttl_secs`), blocking other sessions of
/// the same family from using it even though the slot is already free.
///
/// Only `Provisioning` custom videos represent a real provider handoff that benefits from
/// keeping the same provider pinned, and real provider streams (`stream_info` carries no
/// `CustomVideoStreamType`) obviously qualify.
pub(crate) fn should_pin_provider_for_session(
    stream_details: &StreamDetails,
    _app_state: &Arc<AppState>,
    _item_type: PlaylistItemType,
) -> bool {
    !matches!(
        stream_details.stream_info.as_ref(),
        Some((_, _, _, Some(cv))) if *cv != CustomVideoStreamType::Provisioning
    )
}

pub fn create_session_fingerprint(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    socket_bound: bool,
) -> String {
    if socket_bound {
        concat_string!(&fingerprint.addr.to_string(), "|", username, "|", &virtual_id.to_string())
    } else {
        concat_string!(&fingerprint.key, "|", username, "|", &virtual_id.to_string())
    }
}

pub(crate) fn create_playback_session_fingerprint(
    fingerprint: &Fingerprint,
    username: &str,
    virtual_id: u32,
    item_type: PlaylistItemType,
    extension: Option<&str>,
) -> String {
    // This scopes the session identity, not the session address-tracking policy.
    // Adaptive and seekable playback use the stable client fingerprint so follow-up
    // requests on new sockets reuse the same logical session. Plain live playback
    // remains socket-bound so a separate connection creates a separate session.
    let session_bound = is_session_based_playback(item_type, extension);
    let socket_bound = !session_bound && is_socket_bound_playback_session(item_type, extension);
    create_session_fingerprint(fingerprint, username, virtual_id, socket_bound)
}

pub(crate) fn is_session_based_playback(item_type: PlaylistItemType, extension: Option<&str>) -> bool {
    item_type.is_live_adaptive() || matches!(extension, Some(ext) if ext == HLS_EXT || ext == DASH_EXT)
}

pub(crate) fn is_socket_bound_playback_session(item_type: PlaylistItemType, extension: Option<&str>) -> bool {
    item_type.uses_socket_bound_session() && !is_session_based_playback(item_type, extension)
}

pub(super) fn session_reacquire_cleanup_addrs(
    user_session: &UserSession,
    current_addr: &SocketAddr,
) -> Vec<SocketAddr> {
    let mut addrs: SmallVec<[SocketAddr; 4]> = SmallVec::new();
    if user_session.addr != *current_addr {
        addrs.push(user_session.addr);
    }
    for addr in &user_session.active_addrs {
        if *addr != *current_addr && !addrs.contains(addr) {
            addrs.push(*addr);
        }
    }
    addrs.into_vec()
}
