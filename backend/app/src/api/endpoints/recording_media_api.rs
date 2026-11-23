//! Authorized recording media routes.
//!
//! Three HTTP routes serve completed/recorded media after the
//! `recording_catalog_access` policy gate authorizes the request
//! (private to owner, shared to anyone with `recording.read`,
//! `LegacyAdmin` to admins, orphans to admins). The relative path
//! is taken from the persisted task metadata — never from the URL —
//! and re-validated at open time with `recording_paths`.
//!
//! The deletion/playback race: an already-opened stream may finish
//! where the OS permits; a new open after `Deleting` is denied.
//! `authorize_open` enforces the `Deleting` check, and the file-open
//! step below catches the actual disappearance (404).
//!

use crate::{
    api::model::{
        recording_catalog_access::{self, CatalogAccessError},
        AppState, RecordingQueue,
    },
    auth::{AuthBearer, AuthError},
    utils::no_follow_path_in_root,
};
use axum::{
    body::Body,
    extract::{FromRequestParts, Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    RequestPartsExt,
};
use shared::model::{Claims, RoleSet, UserId, CURRENT_PERMISSION_SCHEMA_VERSION, PERM_ALL, TOKEN_NO_AUTH};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// `AuthClaims` extracts the authenticated `Claims` from a bearer
/// token. The recording policy gate (T13) needs the full `Claims`,
/// not just a permission bit, because the visibility/private-owner
/// check runs against `subject_id` and `roles`.
#[derive(Debug)]
pub struct AuthClaims(pub Claims);

fn builtin_admin_claims() -> Claims {
    Claims {
        username: "admin".to_string(),
        iss: "tuliprox".to_string(),
        iat: 0,
        exp: i64::MAX,
        roles: RoleSet::ADMIN,
        permissions: PERM_ALL,
        pwd_version: 0,
        subject_id: Some(UserId::builtin_admin()),
        permission_schema_version: CURRENT_PERMISSION_SCHEMA_VERSION,
    }
}

fn auth_claims_rejection(error: AuthError) -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    if error.is_token_refresh_required() {
        response.headers_mut().insert("X-Token-Refresh", header::HeaderValue::from_static("required"));
    }
    response
}

impl FromRequestParts<Arc<AppState>> for AuthClaims {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let app_state = state.clone();
        let AuthBearer(token) =
            parts.extract::<AuthBearer>().await.map_err(axum::response::IntoResponse::into_response)?;
        let config = app_state.app_config.config.load();
        match config.web_ui.as_ref().and_then(|w| w.auth.as_ref()).filter(|auth| auth.enabled) {
            Some(_web_auth) => {
                let mut claims = crate::api::auth_middleware::authenticate(&app_state, &token)
                    .await
                    .map_err(auth_claims_rejection)?;
                claims.permissions = crate::api::auth_middleware::effective_permissions(&app_state, &claims);
                Ok(Self(claims))
            }
            None if token == TOKEN_NO_AUTH => Ok(Self(builtin_admin_claims())),
            None => Err((StatusCode::UNAUTHORIZED, "invalid token").into_response()),
        }
    }
}

impl AuthClaims {
    // Helper used by the resolve_for_open path. Kept as a free
    // function on `AuthClaims` (not a method) to avoid borrowing
    // `self` when callers only have a `&Claims` in scope.
}

/// One contiguous slice of media for a clip: either taken from the synthesized
/// header buffer or from the underlying physical file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VirtualClipSegment {
    Header { start: usize, len: usize },
    File { offset: u64, len: u64 },
}

/// Pure mapping function that translates a requested virtual byte range
/// `[virtual_start, virtual_start + virtual_len)` of a clip into a list of
/// contiguous explicit segments (`Header` from the header buffer, `File` from the physical file).
pub fn map_virtual_clip_range(
    virtual_start: u64,
    virtual_len: u64,
    header_len: u64,
    clip_start_offset: u64,
    clip_len: u64,
) -> Vec<VirtualClipSegment> {
    if virtual_len == 0 {
        return Vec::new();
    }
    let total_virtual_len = header_len.saturating_add(clip_len);
    if virtual_start >= total_virtual_len {
        return Vec::new();
    }
    let effective_len = virtual_len.min(total_virtual_len - virtual_start);
    let mut segments = Vec::with_capacity(2);

    if virtual_start < header_len {
        let h_start = usize::try_from(virtual_start).unwrap_or(0);
        let h_take = usize::try_from(effective_len.min(header_len - virtual_start)).unwrap_or(0);
        if h_take > 0 {
            segments.push(VirtualClipSegment::Header { start: h_start, len: h_take });
        }
    }

    let virtual_end = virtual_start + effective_len;
    if virtual_end > header_len {
        let p_start_in_clip = virtual_start.saturating_sub(header_len);
        let p_file_offset = clip_start_offset + p_start_in_clip;
        let p_len = virtual_end - header_len.max(virtual_start);
        let p_take = p_len.min(clip_len.saturating_sub(p_start_in_clip));
        if p_take > 0 {
            segments.push(VirtualClipSegment::File { offset: p_file_offset, len: p_take });
        }
    }

    segments
}

/// Resolved target for a media request: the absolute path on disk,
/// the recording root it was resolved against, and the file size for
/// the response headers. The root travels alongside the `abs_path` so
/// every later re-validation (between `File::open` and the actual
/// byte read) can re-check that no intermediate component has been
/// swapped in as a symlink.
pub(crate) struct ResolvedMedia {
    abs_path: PathBuf,
    recording_root: PathBuf,
    size: u64,
    clip: Option<tuliprox_repository::recording_repository::RecordingClip>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeSpec {
    /// Open-ended suffix: `bytes=-N` (last N bytes)
    Suffix(u64),
    /// Closed range: `bytes=START-END`
    Closed { start: u64, end: u64 },
}

impl RangeSpec {
    /// `total` is the file size. Returns the absolute `(start, length)`
    /// for the partial response, or `None` for "range not satisfiable".
    fn resolve(self, total: u64) -> Option<(u64, u64)> {
        if total == 0 {
            return None;
        }
        match self {
            Self::Suffix(n) => {
                if n == 0 {
                    return None;
                }
                let n = n.min(total);
                Some((total - n, n))
            }
            Self::Closed { start, end } => {
                if start >= total {
                    return None;
                }
                let end = end.min(total - 1);
                if end < start {
                    return None;
                }
                Some((start, end - start + 1))
            }
        }
    }
}

fn parse_range(header_value: &str, total: u64) -> Option<RangeSpec> {
    // Only `bytes=...` is meaningful for media streaming. HTTP
    // ranges are not specified for multi-range; we support a single
    // range only (multi-range would need a
    // multipart/byteranges response, which the frontends do not
    // request yet).
    let rest = header_value.strip_prefix("bytes=")?;
    let mut parts = rest.splitn(2, '-');
    let start = parts.next()?.trim();
    let end = parts.next()?.trim();
    if start.is_empty() {
        // Suffix: `bytes=-N`. An oversized N (n > total) is allowed here
        // and clamped to `total` inside `RangeSpec::resolve` so the
        // client receives the full representation rather than a hard
        // error. Zero is handled in `resolve` so that the parser
        // remains a structural check only.
        let n: u64 = end.parse().ok()?;
        Some(RangeSpec::Suffix(n))
    } else {
        let s: u64 = start.parse().ok()?;
        if end.is_empty() {
            // Open-ended: `bytes=START-` → from START to EOF
            if s >= total {
                return None;
            }
            Some(RangeSpec::Closed { start: s, end: total - 1 })
        } else {
            let e: u64 = end.parse().ok()?;
            Some(RangeSpec::Closed { start: s, end: e })
        }
    }
}

fn access_error_to_response(err: &CatalogAccessError) -> Response {
    // Prevent path disclosure in errors and logs: every
    // CatalogAccessError is mapped to a generic status + the stable
    // `recording_*` code; no path or owner id is leaked.
    let code = err.code();
    if matches!(err, CatalogAccessError::TokenRefreshRequired) {
        // T12 contract: stale schema → 401 + X-Token-Refresh.
        let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": code}))).into_response();
        resp.headers_mut().insert("X-Token-Refresh", header::HeaderValue::from_static("required"));
        return resp;
    }
    let status = match err {
        CatalogAccessError::TokenRefreshRequired => StatusCode::UNAUTHORIZED,
        CatalogAccessError::MissingPermission | CatalogAccessError::Forbidden => StatusCode::FORBIDDEN,
        CatalogAccessError::NotFound => StatusCode::NOT_FOUND,
        CatalogAccessError::InvalidPath => StatusCode::BAD_REQUEST,
        CatalogAccessError::InDeletingState => StatusCode::CONFLICT,
        CatalogAccessError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, axum::Json(serde_json::json!({"error": code}))).into_response()
}

/// Find the recording, authorize the open, and resolve the on-disk
/// path. Every step is a security boundary; no step logs the path.
///
/// `visibility` lets a caller that already judged this recording reuse its
/// request-scoped [`AppRecordingVisibility`] (and its per-item cache). `None` builds
/// a fresh one, so the check itself always runs.
pub(crate) async fn resolve_for_open<'a>(
    app_state: &'a AppState,
    claims: &Claims,
    uuid: &str,
    visibility: Option<&'a crate::api::endpoints::recording_api::AppRecordingVisibility<'a>>,
) -> Result<ResolvedMedia, Box<Response>> {
    let queue: &RecordingQueue = &app_state.recording.queue;
    let recording = recording_catalog_access::lookup_recording(queue, uuid)
        .await
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::NotFound)))?;
    let meta = &recording.recording;
    let relative = meta
        .relative_path
        .as_deref()
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::InvalidPath)))?;
    let subject_id = claims
        .subject_id
        .as_ref()
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::TokenRefreshRequired)))?;
    let owned_visibility;
    let visibility: Option<&dyn recording_catalog_access::RecordingVisibilityPort> =
        if let Some(visibility) = visibility {
            Some(visibility)
        } else {
            owned_visibility = crate::api::endpoints::recording_api::AppRecordingVisibility::new(app_state);
            Some(&owned_visibility)
        };
    recording_catalog_access::authorize_open(
        queue,
        claims,
        subject_id,
        uuid,
        Path::new(relative),
        true, // existence/type is re-checked below with no_follow_regular_file
        visibility,
    )
    .await
    .map_err(|e| Box::new(access_error_to_response(&e)))?;
    let config = app_state.app_config.config.load();
    let recording_root = config.recording().map(|recording| recording.directory.clone()).ok_or_else(|| {
        Box::new(
            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": "recording_not_configured"})))
                .into_response(),
        )
    })?;
    let recording_root = PathBuf::from(recording_root);
    // The stored path is relative to the recording root and carries no owner
    // or visibility component: one physical file is shared by everyone who
    // requested it, so its location cannot depend on who asked first.
    let target_path = tuliprox_dvr::recording_path::resolve_under_root(&recording_root, Path::new(relative))
        .map_err(|_| Box::new(access_error_to_response(&CatalogAccessError::InvalidPath)))?;
    // Re-validate the on-disk file is a regular file (no symlink,
    // no directory) at every intermediate component between the
    // configured root and the leaf — otherwise a swapped-in symlink
    // under the recording root would route reads outside it.
    let (abs_path, file_meta) = if let Some(meta) = no_follow_path_in_root(&recording_root, &target_path).await {
        (target_path.clone(), meta)
    } else {
        let partial = tuliprox_dvr::recording_worker::recording_partial_path(&target_path);
        let meta = no_follow_path_in_root(&recording_root, &partial)
            .await
            .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::NotFound)))?;
        (partial, meta)
    };
    let (size, clip) = if let Some(mut clip) = recording.clip {
        let (start, end) = if let (Some(s), Some(e)) = (clip.start_offset, clip.end_offset) {
            (s, e)
        } else {
            let (s, e) = tuliprox_dvr::recording_worker::compute_clip_offsets(&target_path, clip.start_ts, clip.end_ts)
                .await
                .map_err(|_| Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))?;
            (s, e)
        };
        if clip.header_bytes.is_none() {
            clip.header_bytes = Some(
                tuliprox_dvr::recording_worker::read_clip_header(&target_path)
                    .await
                    .map_err(|_| Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))?,
            );
        }
        clip.start_offset = Some(start);
        clip.end_offset = Some(end);
        let virtual_size = (tuliprox_dvr::recording_worker::CLIP_HEADER_LEN as u64) + (end.saturating_sub(start));
        (virtual_size, Some(clip))
    } else {
        (file_meta.len(), None)
    };
    Ok(ResolvedMedia { abs_path, recording_root, size, clip })
}

/// `GET /library/recording/playback/{uuid}` — supports HTTP Range
/// (RFC 7233 single-range form) and full-stream responses.
pub async fn playback_recording(
    State(app_state): State<Arc<AppState>>,
    claims: AuthClaims,
    AxumPath(uuid): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    match resolve_for_open(&app_state, &claims.0, &uuid, None).await {
        Ok(resolved) => serve_range(&app_state, &resolved, &headers, false).await,
        Err(response) => *response,
    }
}

/// `GET /library/recording/download/{uuid}` — `Content-Disposition:
/// attachment` with the sanitized filename.
pub async fn download_recording(
    State(app_state): State<Arc<AppState>>,
    claims: AuthClaims,
    AxumPath(uuid): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    match resolve_for_open(&app_state, &claims.0, &uuid, None).await {
        Ok(resolved) => serve_range(&app_state, &resolved, &headers, true).await,
        Err(response) => *response,
    }
}

/// `GET /library/recording/thumbnail/{uuid}` — not implemented yet;
/// thumbnail generation lands with the dedicated scanner in a later
/// release. Returning 404 (not 501) so legacy clients do not retry
/// forever.
pub async fn thumbnail_recording(
    State(app_state): State<Arc<AppState>>,
    claims: AuthClaims,
    AxumPath(uuid): AxumPath<String>,
) -> Response {
    let Some(subject_id) = claims.0.subject_id.as_ref() else {
        return access_error_to_response(&CatalogAccessError::TokenRefreshRequired);
    };
    let Some(task) =
        tuliprox_dvr::recording::recording_catalog_access::lookup_recording(&app_state.recording.queue, &uuid).await
    else {
        return access_error_to_response(&CatalogAccessError::NotFound);
    };
    let visibility = crate::api::endpoints::recording_api::AppRecordingVisibility::new(&app_state);
    if !tuliprox_dvr::recording::recording_catalog_access::RecordingVisibilityPort::is_visible(
        &visibility,
        &task.recording,
        &claims.0,
        subject_id,
    )
    .await
    {
        return access_error_to_response(&CatalogAccessError::Forbidden);
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn open_clip_segment_stream(
    path: &Path,
    header_buf: &[u8],
    segment: VirtualClipSegment,
) -> std::io::Result<futures::stream::BoxStream<'static, Result<axum::body::Bytes, std::io::Error>>> {
    use futures::StreamExt;
    match segment {
        VirtualClipSegment::Header { start, len } => {
            let end = start.saturating_add(len).min(header_buf.len());
            let start = start.min(end);
            let data = axum::body::Bytes::copy_from_slice(&header_buf[start..end]);
            Ok(futures::stream::once(async move { Ok(data) }).boxed())
        }
        VirtualClipSegment::File { offset, len } => {
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            Ok(ReaderStream::new(file.take(len)).boxed())
        }
    }
}

async fn stream_clip_segments(
    path: &Path,
    header_buf: &[u8],
    segments: &[VirtualClipSegment],
) -> std::io::Result<Body> {
    use futures::StreamExt;
    let mut chained: Option<futures::stream::BoxStream<'static, Result<axum::body::Bytes, std::io::Error>>> = None;
    for &segment in segments {
        let stream = open_clip_segment_stream(path, header_buf, segment).await?;
        chained = Some(match chained {
            Some(prev) => prev.chain(stream).boxed(),
            None => stream,
        });
    }
    match chained {
        Some(s) => Ok(Body::from_stream(s)),
        None => Ok(Body::empty()),
    }
}

async fn build_clip_body(
    resolved: &ResolvedMedia,
    clip: &tuliprox_repository::recording_repository::RecordingClip,
    start: u64,
    length: u64,
) -> Result<Body, StatusCode> {
    let start_offset = clip.start_offset.unwrap_or(0);
    let end_offset = clip.end_offset.unwrap_or(start_offset);
    let clip_len = end_offset.saturating_sub(start_offset);
    let segments = map_virtual_clip_range(
        start,
        length,
        tuliprox_dvr::recording_worker::CLIP_HEADER_LEN as u64,
        start_offset,
        clip_len,
    );
    let header_buf = clip.header_bytes.as_deref().unwrap_or(&[]);
    stream_clip_segments(&resolved.abs_path, header_buf, &segments).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub(crate) async fn serve_range(
    _app_state: &Arc<AppState>,
    resolved: &ResolvedMedia,
    headers: &HeaderMap,
    attachment: bool,
) -> Response {
    match serve_range_parts(resolved, headers, attachment, None).await {
        Ok((status, response_headers, body)) => build_response(status, response_headers, body),
        Err(response) => *response,
    }
}

/// The status, headers and body of a media response for `resolved`.
///
/// `content_type` overrides the byte-stream default for callers that know the
/// container from the filename. Every range and clip rule lives here, so the
/// Xtream movie path and the library playback path cannot drift apart.
pub(crate) async fn serve_range_parts(
    resolved: &ResolvedMedia,
    headers: &HeaderMap,
    attachment: bool,
    content_type: Option<&str>,
) -> Result<(StatusCode, HeaderMap, Body), Box<Response>> {
    let total = resolved.size;
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let filename = resolved.abs_path.file_name().and_then(|s| s.to_str()).unwrap_or("recording");
    let mut base_headers = HeaderMap::new();
    let content_type_val = match content_type {
        Some(ct) => {
            HeaderValue::from_str(ct).map_err(|_| Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))?
        }
        None => HeaderValue::from_static("application/octet-stream"),
    };
    base_headers.insert(header::CONTENT_TYPE, content_type_val);
    base_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if attachment {
        let disp = HeaderValue::from_str(&attachment_disposition(filename))
            .map_err(|_| Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))?;
        base_headers.insert(header::CONTENT_DISPOSITION, disp);
    }
    if let Some(rh) = range_header {
        let Some(spec) = parse_range(rh, total) else {
            // RFC 7233 §4.4: 416 with `Content-Range: bytes */<total>`.
            let mut err_headers = HeaderMap::new();
            if let Ok(cr) = HeaderValue::from_str(&format!("bytes */{total}")) {
                err_headers.insert(header::CONTENT_RANGE, cr);
            }
            return Err(Box::new((StatusCode::RANGE_NOT_SATISFIABLE, err_headers).into_response()));
        };
        let Some((start, length)) = spec.resolve(total) else {
            let mut err_headers = HeaderMap::new();
            if let Ok(cr) = HeaderValue::from_str(&format!("bytes */{total}")) {
                err_headers.insert(header::CONTENT_RANGE, cr);
            }
            return Err(Box::new((StatusCode::RANGE_NOT_SATISFIABLE, err_headers).into_response()));
        };
        // Race rule: re-validate no component between root and the
        // leaf has been swapped in as a symlink since `resolve_for_open`
        // approved the open.
        if no_follow_path_in_root(&resolved.recording_root, &resolved.abs_path).await.is_none() {
            return Err(Box::new(access_error_to_response(&CatalogAccessError::NotFound)));
        }
        let content_range = HeaderValue::from_str(&format!("bytes {start}-{}/{total}", start + length - 1))
            .map_err(|_| Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()))?;
        let mut hdrs = base_headers.clone();
        hdrs.insert(header::CONTENT_RANGE, content_range);
        hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        if let Some(ref clip) = resolved.clip {
            let Ok(body) = build_clip_body(resolved, clip, start, length).await else {
                return Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
            };
            return Ok((StatusCode::PARTIAL_CONTENT, hdrs, body));
        }
        let Ok(file) = tokio::fs::File::open(&resolved.abs_path).await else {
            return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
        };
        let mut seeked = file;
        if seeked.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
        }
        let limited = seeked.take(length);
        let stream = ReaderStream::new(limited);
        Ok((StatusCode::PARTIAL_CONTENT, hdrs, Body::from_stream(stream)))
    } else {
        // No Range header → full body. The file was already
        // re-validated at open time in `resolve_for_open`. The byte
        // limit mirrors the range path: it caps the stream at the
        // advertised Content-Length so a concurrent append or symlink
        // swap cannot overshoot the response.
        if no_follow_path_in_root(&resolved.recording_root, &resolved.abs_path).await.is_none() {
            return Err(Box::new(access_error_to_response(&CatalogAccessError::NotFound)));
        }
        let mut hdrs = base_headers.clone();
        hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from(total));
        if let Some(ref clip) = resolved.clip {
            let Ok(body) = build_clip_body(resolved, clip, 0, total).await else {
                return Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()));
            };
            return Ok((StatusCode::OK, hdrs, body));
        }
        let Ok(file) = tokio::fs::File::open(&resolved.abs_path).await else {
            return Err(Box::new(StatusCode::NOT_FOUND.into_response()));
        };
        let limited = file.take(total);
        let stream = ReaderStream::new(limited);
        Ok((StatusCode::OK, hdrs, Body::from_stream(stream)))
    }
}

/// `attachment` disposition for a filename that may contain letters of any
/// script (RFC 6266): an ASCII-only `filename` for legacy clients plus the
/// exact UTF-8 name in `filename*`.
fn attachment_disposition(filename: &str) -> String {
    if filename.is_ascii() {
        return format!("attachment; filename=\"{filename}\"");
    }
    let fallback: String = filename.chars().map(|c| if c.is_ascii() { c } else { '_' }).collect();
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        shared::utils::percent_encode_unreserved(filename)
    )
}

pub(crate) fn build_response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Register the recording media routes under `/library/recording/...`.
///
/// Auth is enforced by the `AuthClaims` extractor inside each
/// handler. The recording policy gate
/// (`recording_catalog_access::authorize_open`) is the second gate
/// that enforces ownership and visibility.
pub fn recording_media_api_register(router: axum::Router<Arc<AppState>>) -> axum::Router<Arc<AppState>> {
    router
        .route("/library/recording/playback/{uuid}", get(playback_recording))
        .route("/library/recording/download/{uuid}", get(download_recording))
        .route("/library/recording/thumbnail/{uuid}", get(thumbnail_recording))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, auth::create_jwt_admin, model::Config};
    use axum::http::Request;
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use shared::model::{
        Permission, RoleSet, UserId, WebUiConfigDto, CURRENT_PERMISSION_SCHEMA_VERSION, PERM_ALL, TOKEN_NO_AUTH,
    };
    use tower::ServiceExt;

    fn config_with_web_auth(enabled: bool, secret: &str) -> Config {
        let web_ui = WebUiConfigDto {
            auth: Some(shared::model::WebAuthConfigDto {
                enabled,
                issuer: "test".to_string(),
                secret: secret.to_string(),
                ..shared::model::WebAuthConfigDto::default()
            }),
            ..shared::model::WebUiConfigDto::default()
        };
        Config { web_ui: Some((&web_ui).into()), ..Config::default() }
    }

    async fn extract_auth_claims(
        state: &Arc<AppState>,
        authorization: Option<&str>,
    ) -> Result<AuthClaims, Box<Response>> {
        let mut builder = Request::builder();
        if let Some(value) = authorization {
            builder = builder.header(header::AUTHORIZATION, value);
        }
        let request = builder.body(()).expect("request");
        let (mut parts, ()) = request.into_parts();
        AuthClaims::from_request_parts(&mut parts, state).await.map_err(Box::new)
    }

    #[tokio::test]
    async fn auth_claims_accepts_only_dummy_bearer_without_web_auth() {
        let state = create_test_app_state(Config::default());
        let AuthClaims(claims) =
            extract_auth_claims(&state, Some(&format!("Bearer {TOKEN_NO_AUTH}"))).await.expect("builtin claims");

        assert_eq!(claims.subject_id, Some(UserId::builtin_admin()));
        assert_eq!(claims.roles, RoleSet::ADMIN);
        assert_eq!(claims.permissions, PERM_ALL);
        assert_eq!(claims.permission_schema_version, CURRENT_PERMISSION_SCHEMA_VERSION);
        assert!(claims.permissions.contains(Permission::RecordingManage));

        for authorization in [None, Some("Basic authorized"), Some("Bearer wrong")] {
            let response = extract_auth_claims(&state, authorization).await.expect_err("rejected");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert!(!response.headers().contains_key("X-Token-Refresh"));
        }
    }

    #[tokio::test]
    async fn auth_claims_accepts_dummy_bearer_with_web_auth_disabled() {
        let state = create_test_app_state(config_with_web_auth(false, "unused"));

        let AuthClaims(claims) =
            extract_auth_claims(&state, Some(&format!("Bearer {TOKEN_NO_AUTH}"))).await.expect("builtin claims");

        assert_eq!(claims.subject_id, Some(UserId::builtin_admin()));
        assert_eq!(claims.permissions, PERM_ALL);
    }

    #[tokio::test]
    async fn auth_claims_rejects_dummy_bearer_with_web_auth_enabled() {
        let state = create_test_app_state(config_with_web_auth(true, "secret"));

        let response =
            extract_auth_claims(&state, Some(&format!("Bearer {TOKEN_NO_AUTH}"))).await.expect_err("dummy rejected");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!response.headers().contains_key("X-Token-Refresh"));
    }

    fn test_claims_for_issuer(issuer: &str) -> Claims {
        let mut claims = builtin_admin_claims();
        claims.iss = issuer.to_string();
        claims
    }

    #[tokio::test]
    async fn auth_claims_enabled_jwt_truth_table() {
        let config = config_with_web_auth(true, "secret");
        let web_auth = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).expect("web auth").clone();
        let state = create_test_app_state(config);
        let valid = create_jwt_admin(&web_auth, "admin", 0).expect("jwt");

        let AuthClaims(claims) =
            extract_auth_claims(&state, Some(&format!("Bearer {valid}"))).await.expect("valid claims");
        assert_eq!(claims.subject_id, Some(UserId::builtin_admin()));

        let mut stale_claims = test_claims_for_issuer(&web_auth.issuer);
        stale_claims.permission_schema_version = 0;

        let mut missing_subject = test_claims_for_issuer(&web_auth.issuer);
        missing_subject.subject_id = None;

        for claims in [stale_claims, missing_subject] {
            let token =
                encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(b"secret")).expect("jwt");
            let response =
                extract_auth_claims(&state, Some(&format!("Bearer {token}"))).await.expect_err("refresh required");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers().get("X-Token-Refresh").and_then(|value| value.to_str().ok()),
                Some("required")
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn recording_requests_authentication_and_live_permissions() -> Result<(), Box<dyn std::error::Error>> {
        let recording_dto = shared::model::RecordingConfigDto { enabled: true, ..Default::default() };
        let recording_runtime = crate::model::RecordingConfig::from(&recording_dto);
        let alice_pwd_version = crate::model::WebAuthConfig::pwd_version_from_hash("alice-secret");
        let web_ui_dto = WebUiConfigDto {
            auth: Some(shared::model::WebAuthConfigDto {
                enabled: true,
                issuer: "test".to_string(),
                secret: "0123456789abcdef0123456789abcdef".to_string(),
                ..shared::model::WebAuthConfigDto::default()
            }),
            user_ui_enabled: true,
            ..shared::model::WebUiConfigDto::default()
        };
        let mut web_ui = crate::model::WebUiConfig::from(&web_ui_dto);
        if let Some(auth) = web_ui.auth.as_mut() {
            auth.t_users = Some(vec![crate::model::WebUiUser {
                username: "alice".to_string(),
                password_hash: "alice-secret".to_string(),
                groups: vec!["admin".to_string()],
            }]);
        }
        let config = crate::model::Config {
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(recording_runtime),
            }),
            web_ui: Some(web_ui),
            ..crate::model::Config::default()
        };
        let state = create_test_app_state(config);
        let web_auth = state.app_config.config.load().web_ui.as_ref().unwrap().auth.as_ref().unwrap().clone();

        let api_proxy_dto = shared::model::ApiProxyConfigDto {
            user: vec![shared::model::TargetUserDto {
                target: "main".to_string(),
                credentials: vec![
                    shared::model::ProxyUserCredentialsDto {
                        username: "player_private".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::Private),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    shared::model::ProxyUserCredentialsDto {
                        username: "player_none".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        player_features: Some(shared::model::PlayerFeaturesDto {
                            recording: Some(shared::model::PlayerRecordingLevel::None),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let mut api_proxy = crate::model::ApiProxyConfig::from(&api_proxy_dto);
        for tu in &mut api_proxy.user {
            for cred in &mut tu.credentials {
                Arc::make_mut(cred).resolve_plan(&std::collections::HashMap::new());
            }
        }
        state.app_config.set_api_proxy(api_proxy).unwrap();

        let recording_config = crate::model::RecordingConfig::from(&recording_dto);
        let metadata = shared::model::recording::RecordingMetadata::new_media(
            shared::model::recording::RecordingOwner::User(UserId::api("player_private")),
            shared::model::recording::RecordingVisibility::Private,
            shared::model::recording::RecordingSource::new("target", "42", "provider"),
            "Film".to_string(),
        );
        let mut task = crate::api::model::RecordingTask::new(
            shared::model::RecordingKind::Vod,
            "https://example.com/film.mp4",
            "film.mp4",
            &recording_config,
            Some(Arc::from("provider")),
            recording_config.priority,
            metadata,
        )
        .expect("valid task");
        task.uuid = "rec-uuid-1".to_string();
        state.recording.queue.finished.write().await.push(task);

        let router = crate::api::endpoints::v1_api::v1_api_register(true, &state, "").with_state(Arc::clone(&state));

        let body = serde_json::json!({
            "source": {
                "target_id": "main",
                "virtual_id": "42",
                "cluster": "Live",
                "input_name": "input-a"
            },
            "program_title": "Test",
            "program_start": 1000,
            "program_end": 2000,
            "visibility": "private"
        });

        // 1. Revoked token on POST /api/v1/recording/requests -> 401
        let alice_token =
            crate::auth::create_jwt_web_user(&web_auth, "alice", PERM_ALL, alice_pwd_version, UserId::web("alice"))?;
        state.auth.token_revocations.revoke_subject(&UserId::web("alice"), i64::MAX).await?;
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/recording/requests")
            .header(header::AUTHORIZATION, format!("Bearer {alice_token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body)?))?;
        let res = router.clone().oneshot(req).await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 2. Password-version mismatch -> 401 with existing refresh semantics
        let wrong_pwd_token = crate::auth::create_jwt_web_user(
            &web_auth,
            "alice",
            PERM_ALL,
            alice_pwd_version + 1,
            UserId::web("alice"),
        )?;
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/recording/requests")
            .header(header::AUTHORIZATION, format!("Bearer {wrong_pwd_token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body)?))?;
        let res = router.clone().oneshot(req).await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // 3. API user with recording: private reaches route (no longer 403 missing permission)
        let player_priv_token =
            crate::auth::create_jwt_api_user(&web_auth, "player_private", UserId::api("player_private"))?;
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/recording/requests")
            .header(header::AUTHORIZATION, format!("Bearer {player_priv_token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body)?))?;
        let res = router.clone().oneshot(req).await?;
        assert_ne!(res.status(), StatusCode::FORBIDDEN);

        // 4. API user with recording: none -> 403 / recording_missing_permission
        let player_none_token = crate::auth::create_jwt_api_user(&web_auth, "player_none", UserId::api("player_none"))?;
        let req = Request::builder()
            .method("GET")
            .uri("/api/v1/library/recording/playback/rec-uuid-1")
            .header(header::AUTHORIZATION, format!("Bearer {player_none_token}"))
            .body(axum::body::Body::empty())?;
        let res = router.clone().oneshot(req).await?;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await?;
        let body_str = String::from_utf8_lossy(&bytes);
        assert!(
            body_str.contains("recording_missing_permission"),
            "expected recording_missing_permission, got: {body_str}"
        );

        Ok(())
    }

    #[test]
    fn attachment_disposition_encodes_non_ascii_names() {
        assert_eq!(attachment_disposition("news.ts"), "attachment; filename=\"news.ts\"");
        assert_eq!(
            attachment_disposition("Новости.ts"),
            "attachment; filename=\"_______.ts\"; filename*=UTF-8''%D0%9D%D0%BE%D0%B2%D0%BE%D1%81%D1%82%D0%B8.ts"
        );
    }

    #[test]
    fn parse_range_closed_with_both_bounds() {
        let s = parse_range("bytes=100-199", 1000).expect("parse");
        assert_eq!(s.resolve(1000), Some((100, 100)));
    }

    #[test]
    fn parse_range_open_ended_to_eof() {
        let s = parse_range("bytes=500-", 1000).expect("parse");
        assert_eq!(s.resolve(1000), Some((500, 500)));
    }

    #[test]
    fn parse_range_suffix_last_n() {
        let s = parse_range("bytes=-200", 1000).expect("parse");
        assert_eq!(s.resolve(1000), Some((800, 200)));
    }

    #[test]
    fn parse_range_clamps_overflow_end() {
        let s = parse_range("bytes=900-9999", 1000).expect("parse");
        // end clamped to total-1, so 900..=999
        assert_eq!(s.resolve(1000), Some((900, 100)));
    }

    #[test]
    fn parse_range_rejects_start_past_eof() {
        // `bytes=2000-` is past the file end (total=1000); the
        // parser must reject it before `resolve` is even called.
        assert!(parse_range("bytes=2000-", 1000).is_none());
    }

    #[test]
    fn parse_range_rejects_zero_suffix() {
        let s = parse_range("bytes=-0", 1000).expect("parse");
        assert_eq!(s.resolve(1000), None);
    }

    #[test]
    fn parse_range_rejects_non_bytes_unit() {
        assert!(parse_range("items=0-10", 1000).is_none());
    }

    #[test]
    fn parse_range_rejects_malformed() {
        assert!(parse_range("bytes=abc-def", 1000).is_none());
        assert!(parse_range("bytes=", 1000).is_none());
    }

    #[test]
    fn resolved_media_size_is_passed_through() {
        // Sanity: RangeSpec::Closed maps total=0 → None (no
        // satisfiable range for an empty file).
        let s = parse_range("bytes=0-0", 0).expect("parse");
        assert_eq!(s.resolve(0), None);
    }

    #[test]
    fn map_virtual_clip_range_unit_tests() {
        // Virtual file layout: header_len = 376, clip_len = 1000 (clip_start_offset = 10_000).
        // Total virtual size = 1376 bytes.
        let header_len = 376;
        let clip_start = 10_000;
        let clip_len = 1000;

        // 1. At 0 (length 1)
        let segs_0 = map_virtual_clip_range(0, 1, header_len, clip_start, clip_len);
        assert_eq!(segs_0, vec![VirtualClipSegment::Header { start: 0, len: 1 }]);

        // 2. At 1 (length 1)
        let segs_1 = map_virtual_clip_range(1, 1, header_len, clip_start, clip_len);
        assert_eq!(segs_1, vec![VirtualClipSegment::Header { start: 1, len: 1 }]);

        // 3. Crossing boundary: 1 byte before header end to 1 byte into payload (virtual 375..377)
        let segs_cross = map_virtual_clip_range(375, 2, header_len, clip_start, clip_len);
        assert_eq!(
            segs_cross,
            vec![
                VirtualClipSegment::Header { start: 375, len: 1 },
                VirtualClipSegment::File { offset: 10_000, len: 1 },
            ]
        );

        // 4. Exactly at boundary (virtual 376, length 1)
        let segs_bound = map_virtual_clip_range(376, 1, header_len, clip_start, clip_len);
        assert_eq!(segs_bound, vec![VirtualClipSegment::File { offset: 10_000, len: 1 }]);

        // 5. At last byte (virtual 1375, length 1)
        let segs_last = map_virtual_clip_range(1375, 1, header_len, clip_start, clip_len);
        assert_eq!(segs_last, vec![VirtualClipSegment::File { offset: 10_999, len: 1 }]);

        // 6. Full virtual range
        let segs_full = map_virtual_clip_range(0, 1376, header_len, clip_start, clip_len);
        assert_eq!(
            segs_full,
            vec![
                VirtualClipSegment::Header { start: 0, len: 376 },
                VirtualClipSegment::File { offset: 10_000, len: 1000 },
            ]
        );

        // 7. Edge cases: len 0, past EOF
        assert_eq!(map_virtual_clip_range(0, 0, header_len, clip_start, clip_len), vec![]);
        assert_eq!(map_virtual_clip_range(1376, 1, header_len, clip_start, clip_len), vec![]);

        // 8. Clip payload starting below byte 376 (e.g. at 188) is served as File segment
        let segs_below_376 = map_virtual_clip_range(376, 188, header_len, 188, 500);
        assert_eq!(segs_below_376, vec![VirtualClipSegment::File { offset: 188, len: 188 }]);
    }

    #[tokio::test]
    async fn clip_payload_starting_below_byte_376_is_served_from_file() {
        use http_body_util::BodyExt;
        let dir = tempfile::TempDir::new().unwrap();
        let file_path = dir.path().join("payload.ts");
        let file_content = vec![0xAA; 1000];
        tokio::fs::write(&file_path, &file_content).await.unwrap();

        let header_buf = vec![0xBB; 376];
        // Request segment: File at offset 188, len 188 (payload starts below byte 376)
        let segments = vec![VirtualClipSegment::File { offset: 188, len: 188 }];
        let body = stream_clip_segments(&file_path, &header_buf, &segments).await.unwrap();
        let bytes = body.collect().await.unwrap().to_bytes();

        assert_eq!(bytes.len(), 188);
        assert_eq!(bytes.as_ref(), &file_content[188..376]);
        assert_ne!(bytes.as_ref(), &header_buf[188..376]);
    }

    fn make_test_pat() -> Vec<u8> {
        let mut pkt = vec![0xFF; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x40;
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0xB0;
        pkt[7] = 0x0D;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0x00;
        pkt[14] = 0x01;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        pkt
    }

    fn make_test_pmt() -> Vec<u8> {
        let mut pkt = vec![0xFF; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x50;
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x02;
        pkt[6] = 0xB0;
        pkt[7] = 0x12;
        pkt[8] = 0x00;
        pkt[9] = 0x01;
        pkt[10] = 0xC1;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0xE1;
        pkt[14] = 0x00;
        pkt[15] = 0xF0;
        pkt[16] = 0x00;
        pkt[17] = 0x1B;
        pkt[18] = 0xE1;
        pkt[19] = 0x00;
        pkt[20] = 0xF0;
        pkt[21] = 0x00;
        pkt
    }

    fn make_test_video(is_idr: bool) -> Vec<u8> {
        let mut pkt = vec![0x00; 188];
        pkt[0] = 0x47;
        pkt[1] = 0x41;
        pkt[2] = 0x00;
        pkt[3] = 0x10;
        pkt[4] = 0x00;
        pkt[5] = 0x00;
        pkt[6] = 0x01;
        pkt[7] = 0xE0;
        pkt[8] = 0x00;
        pkt[9] = 0x00;
        pkt[10] = 0x80;
        pkt[11] = 0x00;
        pkt[12] = 0x00;
        pkt[13] = 0x00;
        pkt[14] = 0x00;
        pkt[15] = 0x01;
        pkt[16] = if is_idr { 0x05 } else { 0x01 };
        pkt
    }

    #[tokio::test]
    async fn open_clip_while_enclosing_recording_is_partial_returns_clip_window() {
        use http_body_util::BodyExt;
        let dir = tempfile::TempDir::new().unwrap();
        let recording_dir = dir.path().to_str().unwrap().to_string();

        let recording_dto =
            shared::model::RecordingConfigDto { enabled: true, directory: Some(recording_dir), ..Default::default() };
        let recording_runtime = crate::model::RecordingConfig::from(&recording_dto);
        let config = crate::model::Config {
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(recording_runtime),
            }),
            ..crate::model::Config::default()
        };
        let state = create_test_app_state(config);

        let target_path = dir.path().join("enclosing.ts");
        let partial_path = tuliprox_dvr::recording_worker::recording_partial_path(&target_path);

        let mut partial_data = Vec::new();
        partial_data.extend(make_test_pat());
        partial_data.extend(make_test_pmt());
        partial_data.extend(make_test_video(false));
        partial_data.extend(make_test_video(true));
        partial_data.extend(make_test_video(false));
        tokio::fs::write(&partial_path, &partial_data).await.unwrap();

        let idx_path = tuliprox_dvr::recording_sidecar::time_index_path(&target_path);
        tuliprox_dvr::recording_sidecar::append_time_index_entry(&idx_path, 1000, 0).await.unwrap();
        tuliprox_dvr::recording_sidecar::append_time_index_entry(&idx_path, 1010, 376).await.unwrap();
        tuliprox_dvr::recording_sidecar::append_time_index_entry(&idx_path, 1020, 752).await.unwrap();

        let recording_config = crate::model::RecordingConfig::from(&recording_dto);
        let metadata = shared::model::recording::RecordingMetadata::new_live(
            shared::model::recording::RecordingOwner::User(UserId::builtin_admin()),
            shared::model::recording::RecordingVisibility::Private,
            shared::model::recording::RecordingSource::new("target", "42", "provider"),
            1000,
            2000,
            0,
            0,
        );
        let mut task = crate::api::model::RecordingTask::new(
            shared::model::RecordingKind::Live,
            "https://example.com/live",
            "enclosing.ts",
            &recording_config,
            Some(Arc::from("provider")),
            recording_config.priority,
            metadata,
        )
        .expect("valid task");
        task.uuid = "clip-uuid-1".to_string();
        task.recording.relative_path = Some("enclosing.ts".to_string());
        task.clip = Some(tuliprox_repository::recording_repository::RecordingClip {
            start_ts: 1010,
            end_ts: 1020,
            start_offset: None,
            end_offset: None,
            header_bytes: None,
        });
        state.recording.queue.finished.write().await.push(task);

        let claims = builtin_admin_claims();
        let resolved = resolve_for_open(&state, &claims, "clip-uuid-1", None).await.unwrap();
        assert_eq!(resolved.abs_path, partial_path);
        let clip = resolved.clip.as_ref().unwrap();
        assert_eq!(clip.start_offset, Some(564));
        assert_eq!(clip.end_offset, Some(752));
        assert_eq!(resolved.size, (tuliprox_dvr::recording_worker::CLIP_HEADER_LEN as u64) + (752 - 564));

        let res = playback_recording(
            axum::extract::State(state),
            AuthClaims(claims),
            axum::extract::Path("clip-uuid-1".to_string()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_LENGTH).unwrap(),
            &((tuliprox_dvr::recording_worker::CLIP_HEADER_LEN as u64) + (752 - 564)).to_string()
        );
        let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body_bytes.len(), tuliprox_dvr::recording_worker::CLIP_HEADER_LEN + (752 - 564));
    }
}
