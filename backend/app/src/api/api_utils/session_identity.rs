use super::{get_catchup_session_ttl_secs, get_stream_config_u64};
use crate::{
    api::model::{AppState, CustomVideoStreamType, StreamDetails, UserSession},
    auth::Fingerprint,
};
use shared::{
    concat_string,
    defaults::{default_hls_session_ttl_secs, DASH_EXT, HLS_EXT},
    model::PlaylistItemType,
};
use smallvec::SmallVec;
use std::{net::SocketAddr, sync::Arc};

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
