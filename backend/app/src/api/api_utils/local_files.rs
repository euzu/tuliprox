use super::{
    activate_session_before_stream_open, get_stream_throttle, is_socket_bound_playback_session, is_throttled_stream,
    mark_response_as_uncompressed, prepare_stream_metering, record_connect_failed_attempt, reentry_suppressed_response,
    stream_admission_rejected_response, try_unwrap_body, ConnectFailedAttempt, PlaybackRequestClass,
    SessionActivationRequest,
};
use crate::{
    api::{
        model::{
            create_active_client_stream, create_custom_video_stream_response, AppState, CustomVideoStreamType,
            StreamDetails, StreamError, ThrottledStream,
        },
        static_headers::CT_OCTET,
    },
    auth::Fingerprint,
    model::{ConfigInput, ConfigTarget, ProxyUserCredentials},
    utils::{
        async_file_reader, get_file_extension,
        request::{content_type_from_ext, parse_range},
    },
};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use log::{error, info, log_enabled, trace};
use shared::{
    model::{ConnectFailureReason, FailureStage, StreamChannel, UserConnectionPermission, VirtualId},
    utils::{extract_extension_from_url, human_readable_kbps, sanitize_sensitive_info},
};
use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

#[allow(clippy::missing_panics_doc)]
pub async fn serve_file(file_path: &Path, mime_type: String, cache_control: Option<&str>) -> impl IntoResponse + Send {
    match tokio::fs::try_exists(file_path).await {
        Ok(exists) => {
            if !exists {
                return StatusCode::NOT_FOUND.into_response();
            }
        }
        Err(err) => {
            error!("Failed to open file {}, {err:?}", file_path.display());
            return StatusCode::NOT_FOUND.into_response();
        }
    }

    match tokio::fs::File::open(file_path).await {
        Ok(file) => {
            let last_modified = file.metadata().await.ok().and_then(|m| m.modified().ok()).map(|m| {
                let dt: DateTime<Utc> = m.into();
                dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
            });

            let reader = async_file_reader(file);
            let stream = ReaderStream::new(reader);
            let body = Body::from_stream(stream);

            let mut builder = axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime_type)
                .header(header::CACHE_CONTROL, cache_control.unwrap_or("no-cache"));

            if let Some(lm) = last_modified {
                builder = builder.header(header::LAST_MODIFIED, lm);
            }

            try_unwrap_body!(builder.body(body))
        }
        Err(_) => internal_server_error!(),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn local_stream_response(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    pli: StreamChannel,
    req_headers: &HeaderMap,
    input: &ConfigInput,
    _target: &ConfigTarget,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    playback_session_token: Option<&str>,
    request_class: Option<PlaybackRequestClass>,
    check_path: bool,
) -> impl IntoResponse + Send {
    let _transition_guard = if let Some(session_token) = playback_session_token {
        Some(app_state.active_users.acquire_playback_transition(&user.username, session_token).await)
    } else {
        None
    };
    if log_enabled!(log::Level::Trace) {
        trace!("Try to open stream {}", sanitize_sensitive_info(&pli.url));
    }

    let mut connection_permission = connection_permission;
    let mut grace_mode = None;
    if connection_permission == UserConnectionPermission::Exhausted {
        let allow_session_reopen = if let Some(session_token) = playback_session_token {
            user.max_connections > 0
                && app_state
                    .active_users
                    .connection_permission_for_session(
                        &user.username,
                        user.max_connections,
                        user.soft_connections,
                        session_token,
                    )
                    .await
                    != UserConnectionPermission::Exhausted
        } else {
            false
        };
        if !allow_session_reopen {
            record_connect_failed_attempt(ConnectFailedAttempt {
                app_state,
                fingerprint,
                user,
                stream_channel: pli.clone(),
                provider_name: input.name.clone(),
                req_headers,
                reason: ConnectFailureReason::UserConnectionsExhausted,
                failure_stage: FailureStage::Admission,
            });
            return create_custom_video_stream_response(
                &app_state.provider_stream_ctx(),
                &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            )
            .into_response();
        }
        connection_permission = UserConnectionPermission::Allowed;
    }

    let path = PathBuf::from(pli.url.strip_prefix("file://").unwrap_or(&pli.url));

    let Ok(mut file) = tokio::fs::File::open(&path).await else { return StatusCode::NOT_FOUND.into_response() };
    let Ok(opened_metadata) = file.metadata().await else { return internal_server_error!() };

    // Canonicalize and validate the path
    let canonical = match tokio::fs::canonicalize(&path).await {
        Ok(canonical) => canonical,
        Err(err) => {
            error!("Local file path is corrupt {}: {err}", path.display());
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    if check_path {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Ok(canonical_metadata) = tokio::fs::metadata(&canonical).await else { return internal_server_error!() };
            if opened_metadata.dev() != canonical_metadata.dev() || opened_metadata.ino() != canonical_metadata.ino() {
                error!("TOCTOU race detected: file swapped during local_stream_response");
                return StatusCode::FORBIDDEN.into_response();
            }
        }
        #[cfg(windows)]
        match same_windows_file_identity(&file, &canonical).await {
            Ok(true) => {}
            Ok(false) => {
                error!("TOCTOU race detected: file swapped during local_stream_response");
                return StatusCode::FORBIDDEN.into_response();
            }
            Err(err) => {
                error!("Could not verify local file identity {}: {err}", canonical.display());
                return internal_server_error!();
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            error!("Secure local file identity validation is unsupported on this platform");
            return StatusCode::FORBIDDEN.into_response();
        }

        let Some(library_paths) = app_state
            .app_config
            .config
            .load()
            .library
            .as_ref()
            .map(|lib| lib.scan_directories.iter().map(|dir| dir.path.clone()).collect::<Vec<_>>())
        else {
            return StatusCode::NOT_FOUND.into_response();
        };

        // Verify path is within allowed media directories
        // (requires configuration of allowed base paths)
        if !is_path_within_allowed_directories(&canonical, &library_paths) {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    let file_size = opened_metadata.len();

    let range = req_headers.get("range").and_then(|v| v.to_str().ok()).and_then(parse_range);

    let (start, end) = if let Some((req_start, req_end)) = range {
        if file_size == 0 || req_start >= file_size {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        let end = req_end.unwrap_or(file_size - 1).min(file_size - 1);
        if end < req_start {
            return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        }
        (req_start, end)
    } else {
        if file_size == 0 {
            // Serve empty file
            let body = axum::body::Body::empty();
            let mut response = Response::new(body);
            *response.status_mut() = StatusCode::OK;
            let headers = response.headers_mut();
            if let Some(ext) = get_file_extension(&pli.url) {
                let ct = content_type_from_ext(&ext);
                headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            } else {
                headers.insert(header::CONTENT_TYPE, CT_OCTET.clone()); //HeaderValue::from_static("application/octet-stream"));
            }
            headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
            return response.into_response();
        }
        (0, file_size - 1)
    };

    let content_length = end - start + 1;

    if start > 0 {
        if let Err(_err) = file.seek(SeekFrom::Start(start)).await {
            return internal_server_error!();
        }
    }

    let stream =
        ReaderStream::new(file.take(content_length)).map_err(|err| StreamError::Stream(err.to_string())).boxed();
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    let stream = if is_throttled_stream(pli.item_type, throttle_kbps) {
        info!("Stream throttling active: {}", human_readable_kbps(u64::try_from(throttle_kbps).unwrap_or_default()));
        ThrottledStream::new(stream, throttle_kbps).boxed()
    } else {
        stream
    };
    let socket_bound = is_socket_bound_playback_session(pli.item_type, extract_extension_from_url(&pli.url));
    let mut connection_kind = connection_kind;
    if let Some(session_token) = playback_session_token {
        let activation = activate_session_before_stream_open(
            app_state,
            SessionActivationRequest {
                fingerprint,
                input,
                user,
                session_token,
                request_class,
                virtual_id: VirtualId::new(pli.virtual_id),
                item_type: pli.item_type,
                stream_url: &pli.url,
                connection_permission,
                connection_kind,
                granted_grace_mode: None,
                socket_bound,
            },
        )
        .await;
        grace_mode = activation.grace_mode;
        connection_permission = activation.admission.permission();
        connection_kind = activation.admission.kind().unwrap_or(connection_kind);

        if connection_permission == UserConnectionPermission::Exhausted {
            app_state
                .active_users
                .release_unbound_session_reservation(
                    &user.username,
                    session_token,
                    activation.placeholder_transition_version,
                    activation.placeholder_transition_version.is_some(),
                )
                .await;
            if activation.admission.is_reentry_suppressed() {
                return reentry_suppressed_response();
            }
            return create_custom_video_stream_response(
                &app_state.provider_stream_ctx(),
                &fingerprint.addr,
                CustomVideoStreamType::UserConnectionsExhausted,
            )
            .into_response();
        }
    }
    let mut grace_period_options = app_state.get_grace_options();
    if connection_permission != UserConnectionPermission::GracePeriod {
        grace_period_options.period_millis = 0;
    }
    if let Some(resolved_mode) = grace_mode {
        grace_period_options.hold_stream = matches!(resolved_mode, crate::api::model::GraceMode::Hold);
    }
    let resolved_connection_kind = if let Some(session_token) = playback_session_token {
        app_state
            .active_users
            .get_and_update_user_session(&user.username, session_token)
            .await
            .and_then(|session| session.connection_kind)
            .unwrap_or(connection_kind)
    } else {
        connection_kind
    };
    if let Some(session_token) = playback_session_token {
        let _ = app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token,
                virtual_id: pli.virtual_id,
                provider: input.name.as_ref(),
                stream_url: &pli.url,
                addr: &fingerprint.addr,
                connection_permission,
                connection_kind: Some(resolved_connection_kind),
                socket_bound,
            })
            .await;
    }
    let metering = prepare_stream_metering(app_state, &pli.url, false, true, false);
    let stream = match create_active_client_stream(crate::api::model::ActiveClientStreamParams {
        stream_details: StreamDetails::from_stream(stream, grace_period_options),
        app_state,
        user,
        connection_permission,
        connection_kind: resolved_connection_kind,
        fingerprint,
        stream_channel: pli.clone(),
        socket_bound,
        session_token: playback_session_token,
        req_headers,
        meter_uid: metering.meter_uid,
        meter_stream: metering.meter_stream,
    })
    .await
    {
        Ok(stream) => stream,
        Err(error) => return stream_admission_rejected_response(error, &user.username),
    };

    let mut response = Response::new(axum::body::Body::from_stream(stream));

    *response.status_mut() = if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };

    let headers = response.headers_mut();
    if let Some(ext) = get_file_extension(&pli.url) {
        let ct = content_type_from_ext(&ext);
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
    } else {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    }
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    if let Ok(header_value) = HeaderValue::from_str(&content_length.to_string()) {
        headers.insert(header::CONTENT_LENGTH, header_value);
    }

    if range.is_some() {
        if let Ok(header_value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{file_size}")) {
            headers.insert(header::CONTENT_RANGE, header_value);
        }
    }

    mark_response_as_uncompressed(&mut response);
    response
}

pub(super) fn is_path_within_allowed_directories(sub_path: &Path, root_paths: &[String]) -> bool {
    for root_path in root_paths {
        if sub_path.starts_with(PathBuf::from(root_path)) {
            return true;
        }
    }
    false
}

#[cfg(windows)]
pub(super) async fn same_windows_file_identity(
    opened_file: &tokio::fs::File,
    canonical_path: &Path,
) -> std::io::Result<bool> {
    let canonical_file = tokio::fs::File::open(canonical_path).await?;
    Ok(windows_file_identity(opened_file)? == windows_file_identity(&canonical_file)?)
}

#[cfg(windows)]
pub(super) fn windows_file_identity(file: &tokio::fs::File) -> std::io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    // SAFETY: `file.as_raw_handle()` is a live file handle for the duration of
    // the call, and `info` is a writable output buffer for the WinAPI function.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &raw mut info) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
}
