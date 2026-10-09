use super::{record_connect_failed_attempt, try_unwrap_body, ConnectFailedAttempt};
use crate::{
    api::model::{create_custom_video_stream_response, AppState, CustomVideoStreamType, StreamAdmissionError},
    auth::Fingerprint,
    model::ProxyUserCredentials,
};
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
};
use log::error;
use serde::Serialize;
use shared::{
    model::{ConnectFailureReason, FailureStage, StreamChannel},
    utils::{bin_serialize, CONTENT_TYPE_CBOR},
};
use std::sync::Arc;

pub(super) fn admission_failure_video_type(reason: ConnectFailureReason) -> Option<CustomVideoStreamType> {
    match reason {
        ConnectFailureReason::UserAccountExpired => Some(CustomVideoStreamType::UserAccountExpired),
        ConnectFailureReason::UserConnectionsExhausted => Some(CustomVideoStreamType::UserConnectionsExhausted),
        ConnectFailureReason::ProviderConnectionsExhausted => Some(CustomVideoStreamType::ProviderConnectionsExhausted),
        _ => None,
    }
}

pub(crate) fn admission_failure_response(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    user: &ProxyUserCredentials,
    stream_channel: StreamChannel,
    provider_name: Arc<str>,
    req_headers: &HeaderMap,
    reason: ConnectFailureReason,
) -> axum::response::Response {
    record_connect_failed_attempt(ConnectFailedAttempt {
        app_state,
        fingerprint,
        user,
        stream_channel,
        provider_name,
        req_headers,
        reason,
        failure_stage: FailureStage::Admission,
    });
    let Some(video_type) = admission_failure_video_type(reason) else {
        error!("Unsupported admission failure reason: {reason:?}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    create_custom_video_stream_response(&app_state.provider_stream_ctx(), &fingerprint.addr, video_type).into_response()
}

/// Produces a defined non-success response for a rejected playback request. Unlike the
/// upstream provider response, this carries no Content-Length/Content-Range so downstream
/// players receive an unambiguous error instead of an empty success body.
pub(super) fn stream_admission_rejected_response(
    error: StreamAdmissionError,
    username: &str,
) -> axum::response::Response {
    error!("Stream admission rejected for user {username}: {error:?}");
    axum::response::Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("x-tuliprox-rejection", "admission_rejected")
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

/// Produces a quiet termination response when a recently evicted stream retries.
pub fn reentry_suppressed_response() -> axum::response::Response { StatusCode::NO_CONTENT.into_response() }

pub fn separate_number_and_remainder(input: &str) -> (&str, Option<&str>) {
    input.rfind('.').map_or_else(
        || (input, None),
        |dot_index| {
            let number_part = &input[..dot_index];
            let rest = &input[dot_index..];
            (number_part, if rest.len() < 2 { None } else { Some(rest) })
        },
    )
}

/// # Panics
pub fn empty_json_list_response() -> axum::response::Response {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, crate::api::static_headers::CT_JSON.clone())
        .body("[]".to_owned()))
}

pub fn redirect(url: &str) -> impl IntoResponse {
    try_unwrap_body!(axum::response::Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, url)
        .body(Body::empty()))
}

pub fn bin_response<T: Serialize>(data: &T) -> impl IntoResponse + Send {
    match bin_serialize(data) {
        Ok(body) => ([(header::CONTENT_TYPE, CONTENT_TYPE_CBOR)], body).into_response(),
        Err(_) => internal_server_error!(),
    }
}

pub fn json_response<T: Serialize>(data: &T) -> impl IntoResponse + Send {
    (StatusCode::OK, axum::Json(data)).into_response()
}

pub fn json_or_bin_response<T: Serialize>(accept: Option<&str>, data: &T) -> impl IntoResponse + Send {
    if accept.is_some_and(|a| a.contains(CONTENT_TYPE_CBOR)) {
        return bin_response(data).into_response();
    }
    json_response(data).into_response()
}

pub fn empty_json_response_as_object() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, crate::api::static_headers::CT_JSON.clone())
        .body(axum::body::Body::from("{}".as_bytes()))
}

pub fn empty_json_response_as_array() -> axum::http::Result<axum::response::Response> {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, crate::api::static_headers::CT_JSON.clone())
        .body(axum::body::Body::from("[]".as_bytes()))
}
