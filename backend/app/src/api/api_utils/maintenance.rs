//! Maintenance check: a new playback request for an input in maintenance gets the
//! maintenance clip instead of a provider connection.
//!
//! Requests that continue something already admitted pass: the DVR's internal
//! proxy user (the DVR gate decided before it reserved the slot) and requests
//! carrying a slot reserved for them.

use super::admission_failure_response;
use crate::{
    api::{
        endpoints::hls_api::hls_admission_failure_manifest_response,
        model::{AppState, UserSession},
    },
    auth::Fingerprint,
    model::{ConfigInput, ProxyUserCredentials},
};
use axum::http::HeaderMap;
use shared::model::{ConnectFailureReason, StreamChannel};
use std::sync::Arc;

/// A session that is counted against the user's connections is a running playback; a request
/// for it continues that playback. A known but uncounted session (prepared, preserved, expired)
/// would be admitted anew.
pub(crate) fn is_running_playback(user_session: Option<&UserSession>) -> bool {
    user_session.is_some_and(|session| session.lifecycle.is_counted())
}

/// The maintenance answer for a new playback, given before admission so it neither counts
/// against the user's connections nor evicts anything.
pub(crate) async fn maintenance_rejection_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    stream_channel: StreamChannel,
    input: &ConfigInput,
    req_headers: &HeaderMap,
    is_hls: bool,
) -> axum::response::Response {
    if is_hls {
        return hls_admission_failure_manifest_response(
            app_state,
            fingerprint,
            user,
            stream_channel,
            Arc::clone(&input.name),
            req_headers,
            ConnectFailureReason::Maintenance,
        )
        .await;
    }
    admission_failure_response(
        app_state,
        fingerprint,
        user,
        stream_channel,
        Arc::clone(&input.name),
        req_headers,
        ConnectFailureReason::Maintenance,
    )
}

pub(crate) fn blocks_new_playback(
    app_state: &AppState,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    has_preacquired_handle: bool,
) -> bool {
    !has_preacquired_handle && !user.is_recording_proxy_user() && app_state.maintenance.is_in_maintenance(&input.name)
}
