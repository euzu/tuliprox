use super::{
    activate_session_before_stream_open_with_playback_id, effective_playback_id_for_user, get_stream_throttle,
    is_socket_bound_playback_session, is_throttled_stream, mark_response_as_uncompressed, prepare_stream_metering,
    record_connect_failed_attempt, release_reservation_and_exhausted_response, stream_admission_rejected_response,
    try_unwrap_body, ConnectFailedAttempt, PlaybackRequestClass, SessionActivationRequest,
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
    model::{ConfigInput, ConfigTarget, GracePeriodOptions, ProxyUserCredentials},
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
    path::{Component, Path, PathBuf},
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

/// Refuses a local-file request whose user has no free connection, before any file is opened.
///
/// A session that may reopen past the limit continues as `Allowed`, which is the
/// same decision a fresh request would get.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_local_file_connection(
    fingerprint: &Fingerprint,
    app_state: &Arc<AppState>,
    pli: &StreamChannel,
    req_headers: &HeaderMap,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    playback_session_token: Option<&str>,
) -> Result<UserConnectionPermission, Box<Response<Body>>> {
    if connection_permission != UserConnectionPermission::Exhausted {
        return Ok(connection_permission);
    }
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
    if allow_session_reopen {
        return Ok(UserConnectionPermission::Allowed);
    }
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
    Err(Box::new(
        create_custom_video_stream_response(
            &app_state.provider_stream_ctx(),
            &fingerprint.addr,
            CustomVideoStreamType::UserConnectionsExhausted,
        )
        .into_response(),
    ))
}

/// Registers a local-file byte stream with the session, connection and metering state.
///
/// The returned stream owns the registration: dropping it releases the user
/// connection and the active stream. A user that lost the slot meanwhile gets the
/// exhausted response instead of bytes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn activate_local_file_stream(
    app_state: &Arc<AppState>,
    fingerprint: &Fingerprint,
    pli: &StreamChannel,
    req_headers: &HeaderMap,
    input: &ConfigInput,
    user: &ProxyUserCredentials,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    playback_session_token: Option<&str>,
    request_class: Option<PlaybackRequestClass>,
    stream: crate::api::model::BoxedProviderStream,
) -> Result<crate::api::model::BoxedProviderStream, Box<Response<Body>>> {
    let grace_admitted = connection_permission == UserConnectionPermission::GracePeriod;
    let socket_bound = is_socket_bound_playback_session(pli.item_type, extract_extension_from_url(&pli.url));
    let playback_id = effective_playback_id_for_user(&app_state.app_config.config.load(), user, req_headers);
    let admission = if let Some(session_token) = playback_session_token {
        activate_local_file_session(
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
            playback_id,
        )
        .await?
    } else {
        LocalFileStreamAdmission {
            grace_mode: None,
            placeholder_identity: None,
            connection_permission,
            connection_kind,
        }
    };
    let LocalFileStreamAdmission { grace_mode, mut placeholder_identity, connection_permission, connection_kind } =
        admission;
    let (grace_period_options, resolved_connection_kind) = resolve_local_file_stream_options(
        app_state,
        user,
        playback_session_token,
        connection_permission,
        connection_kind,
        grace_mode,
    )
    .await;
    if let Some(session_token) = playback_session_token {
        let session_params = crate::api::model::CreateUserSessionParams {
            user,
            session_token,
            virtual_id: pli.virtual_id,
            provider: input.name.as_ref(),
            stream_url: &pli.url,
            addr: &fingerprint.addr,
            connection_permission,
            connection_kind: Some(resolved_connection_kind),
            socket_bound,
        };
        placeholder_identity =
            register_local_file_session(app_state, session_params, placeholder_identity, playback_id)
                .await
                .map_err(|error| Box::new(stream_admission_rejected_response(error, &user.username)))?;
    }
    let metering = prepare_stream_metering(app_state, &pli.url, false, true, false);
    let mut stream_details = StreamDetails::from_stream(stream, grace_period_options);
    if let Some(identity) = placeholder_identity {
        let limits_enabled = app_state.app_config.config.load().user_access_control
            && (user.max_connections > 0 || user.soft_connections > 0);
        stream_details.session_registration = Some(tuliprox_session::PlaybackSessionRegistration {
            identity,
            enforce_limits: limits_enabled,
            grace_admitted,
        });
    }
    let outcome = create_active_client_stream(crate::api::model::ActiveClientStreamParams {
        stream_details,
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
        group_failover: None,
    })
    .await
    .map_err(|error| Box::new(stream_admission_rejected_response(error, &user.username)))?;
    Ok(crate::api::model::ActiveClientStreamOutcome::into_stream(outcome))
}

/// Values an activated local-file session contributes to its stream registration.
struct LocalFileStreamAdmission {
    grace_mode: Option<crate::api::model::GraceMode>,
    placeholder_identity: Option<tuliprox_session::SessionIdentity>,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
}

/// Activates the local-file playback session before its first byte is served.
///
/// Returns the admission values the stream registration needs. A session that lost its slot
/// during activation is answered with the exhausted response instead.
async fn activate_local_file_session(
    app_state: &Arc<AppState>,
    request: SessionActivationRequest<'_>,
    playback_id: Option<&str>,
) -> Result<LocalFileStreamAdmission, Box<Response<Body>>> {
    let fingerprint = request.fingerprint;
    let user = request.user;
    let session_token = request.session_token;
    let fallback_kind = request.connection_kind;
    let activation = activate_session_before_stream_open_with_playback_id(app_state, request, playback_id).await;
    let connection_permission = activation.admission.permission();
    let connection_kind = activation.admission.kind().unwrap_or(fallback_kind);
    if connection_permission == UserConnectionPermission::Exhausted {
        let response = release_reservation_and_exhausted_response(
            app_state,
            user,
            session_token,
            activation.placeholder_transition_version,
            activation.admission.is_reentry_suppressed(),
            fingerprint,
        )
        .await;
        return Err(Box::new(response));
    }
    Ok(LocalFileStreamAdmission {
        grace_mode: activation.grace_mode,
        placeholder_identity: activation.placeholder_identity,
        connection_permission,
        connection_kind,
    })
}

/// Resolves the grace period options and connection kind for an activated local-file stream.
///
/// A stream outside the grace period runs without a grace window. A session that already stores
/// a connection kind keeps it.
async fn resolve_local_file_stream_options(
    app_state: &Arc<AppState>,
    user: &ProxyUserCredentials,
    playback_session_token: Option<&str>,
    connection_permission: UserConnectionPermission,
    connection_kind: crate::api::model::ConnectionKind,
    grace_mode: Option<crate::api::model::GraceMode>,
) -> (GracePeriodOptions, crate::api::model::ConnectionKind) {
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
    (grace_period_options, resolved_connection_kind)
}

/// Registers the activated local-file session and returns its refreshed placeholder identity.
///
/// An unfenced registration never fails. A refresh that no longer matches the placeholder is
/// rejected.
async fn register_local_file_session(
    app_state: &Arc<AppState>,
    session_params: crate::api::model::CreateUserSessionParams<'_>,
    placeholder_identity: Option<tuliprox_session::SessionIdentity>,
    playback_id: Option<&str>,
) -> Result<Option<tuliprox_session::SessionIdentity>, crate::api::model::StreamAdmissionError> {
    let Some(identity) = placeholder_identity else {
        let _ = app_state.active_users.create_user_session_with_playback_id(session_params, playback_id).await;
        return Ok(None);
    };
    let Some((_, refreshed_identity)) = app_state
        .active_users
        .create_user_session_checked_with_playback_id(session_params, Some((identity, None)), playback_id)
        .await
    else {
        return Err(crate::api::model::StreamAdmissionError::RegistrationRejected);
    };
    Ok(Some(refreshed_identity))
}

/// Applies the configured stream throttle to a local byte stream.
///
/// Shared by every local-file playback path — the library file itself and a
/// recording served as Xtream VOD — so both honour the same `throttle_kbps` setting.
pub(crate) fn apply_stream_throttle(
    app_state: &Arc<AppState>,
    item_type: shared::model::PlaylistItemType,
    stream: crate::api::model::BoxedProviderStream,
) -> crate::api::model::BoxedProviderStream {
    let throttle_kbps = usize::try_from(get_stream_throttle(app_state)).unwrap_or_default();
    if is_throttled_stream(item_type, throttle_kbps) {
        info!("Stream throttling active: {}", human_readable_kbps(u64::try_from(throttle_kbps).unwrap_or_default()));
        ThrottledStream::new(stream, throttle_kbps).boxed()
    } else {
        stream
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

    let connection_permission = match admit_local_file_connection(
        fingerprint,
        app_state,
        &pli,
        req_headers,
        input,
        user,
        connection_permission,
        playback_session_token,
    )
    .await
    {
        Ok(permission) => permission,
        Err(response) => return *response,
    };

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

        let Some(scan_directories) = app_state.app_config.config.load().library.as_ref().map(|lib| {
            lib.scan_directories.iter().map(|dir| (dir.path.clone(), dir.follow_symlinks)).collect::<Vec<_>>()
        }) else {
            return StatusCode::NOT_FOUND.into_response();
        };

        let mut roots = Vec::with_capacity(scan_directories.len());
        for (dir_path, follow_symlinks) in scan_directories {
            // Only no-follow roots compare canonical paths; skip the syscall otherwise.
            let canonical = if follow_symlinks { None } else { tokio::fs::canonicalize(&dir_path).await.ok() };
            let Ok(lexical) = std::path::absolute(&dir_path) else { continue };
            roots.push(AllowedLibraryRoot { lexical, canonical, follow_symlinks });
        }

        // Verify path is within allowed media directories
        let requested = std::path::absolute(&path).unwrap_or_else(|_| path.clone());
        if !is_library_path_allowed(&requested, &canonical, &roots) {
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
    let stream = apply_stream_throttle(app_state, pli.item_type, stream);
    let stream = match activate_local_file_stream(
        app_state,
        fingerprint,
        &pli,
        req_headers,
        input,
        user,
        connection_permission,
        connection_kind,
        playback_session_token,
        request_class,
        stream,
    )
    .await
    {
        Ok(stream) => stream,
        Err(response) => return *response,
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

/// A library scan directory as seen by the local stream path check.
#[derive(Debug)]
pub(crate) struct AllowedLibraryRoot {
    pub(crate) lexical: PathBuf,
    pub(crate) canonical: Option<PathBuf>,
    pub(crate) follow_symlinks: bool,
}

impl AllowedLibraryRoot {
    /// Applies this root's rule: lexical containment when following symlinks, canonical containment otherwise.
    fn allows(&self, requested: &Path, canonical: &Path) -> bool {
        if self.follow_symlinks {
            // Lexical check: the part below the root must not climb out via `..`.
            requested
                .strip_prefix(&self.lexical)
                .is_ok_and(|rest| rest.components().all(|c| matches!(c, Component::Normal(_))))
        } else {
            self.canonical.as_ref().is_some_and(|root| canonical.starts_with(root))
        }
    }
}

/// The most specific root that lexically contains `requested` decides, so a no-follow root nested inside a
/// follow root stays strict. Without a lexical match, only no-follow roots may accept the canonical path.
pub(crate) fn is_library_path_allowed(requested: &Path, canonical: &Path, roots: &[AllowedLibraryRoot]) -> bool {
    match roots
        .iter()
        .filter(|root| requested.starts_with(&root.lexical))
        .max_by_key(|root| root.lexical.components().count())
    {
        Some(root) => root.allows(requested, canonical),
        None => roots.iter().any(|root| !root.follow_symlinks && root.allows(requested, canonical)),
    }
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
