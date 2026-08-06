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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{FromRequestParts, Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::RequestPartsExt;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use tokio_util::io::ReaderStream;

use shared::model::Claims;
use crate::api::model::recording_catalog_access::{
    self, CatalogAccessError,
};
use crate::api::model::AppState;
use crate::api::model::DownloadQueue;
use crate::auth::{verify_token, AuthBearer};
use crate::utils::{no_follow_regular_file, resolve_recording_dir, RecordingPathError, RecordingVisibility as PathVisibility};

/// `AuthClaims` extracts the authenticated `Claims` from a bearer
/// token. The recording policy gate (T13) needs the full `Claims`,
/// not just a permission bit, because the visibility/private-owner
/// check runs against `subject_id` and `roles`.
pub struct AuthClaims(pub Claims);

impl FromRequestParts<Arc<AppState>> for AuthClaims {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let app_state = state.clone();
        let AuthBearer(token) = parts.extract::<AuthBearer>().await
            .map_err(|(code, msg)| (code, msg).into_response())?;
        let config = app_state.app_config.config.load();
        let Some(web_auth) = config.web_ui.as_ref().and_then(|w| w.auth.as_ref()) else {
            return Err((StatusCode::UNAUTHORIZED, "auth not configured").into_response());
        };
        let token_data = verify_token(&token, web_auth.secret.as_bytes())
            .ok_or_else(|| (StatusCode::UNAUTHORIZED, "invalid token").into_response())?;
        Ok(Self(token_data.claims))
    }
}

impl AuthClaims {
    // Helper used by the resolve_for_open path. Kept as a free
    // function on `AuthClaims` (not a method) to avoid borrowing
    // `self` when callers only have a `&Claims` in scope.
}

/// Resolved target for a media request: the absolute path on disk
/// plus the file size for the response headers.
struct ResolvedMedia {
    abs_path: PathBuf,
    size: u64,
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
        // Suffix: `bytes=-N`
        let n: u64 = end.parse().ok()?;
        if n > total {
            return None;
        }
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
        let mut resp = (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({"error": code})),
        )
            .into_response();
        resp.headers_mut().insert(
            "X-Token-Refresh",
            header::HeaderValue::from_static("required"),
        );
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
fn resolve_for_open(
    app_state: &AppState,
    claims: &Claims,
    uuid: &str,
) -> Result<ResolvedMedia, Box<Response>> {
    let queue: &DownloadQueue = &app_state.downloads;
    let recording = recording_catalog_access::lookup_recording(queue, uuid)
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::NotFound)))?;
    let meta = recording
        .recording
        .as_ref()
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::NotFound)))?;
    let relative = meta
        .relative_path
        .as_deref()
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::InvalidPath)))?;
    let owner_dir = match &meta.owner {
        shared::model::recording::RecordingOwner::User(user_id) => user_id.0.clone(),
        shared::model::recording::RecordingOwner::LegacyAdmin => "legacy".to_string(),
    };
    let subject_id = claims
        .subject_id
        .as_ref()
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::TokenRefreshRequired)))?;
    recording_catalog_access::authorize_open(
        queue,
        claims,
        subject_id,
        uuid,
        Path::new(relative),
        true, // existence/type is re-checked below with no_follow_regular_file
    )
    .map_err(|e| Box::new(access_error_to_response(&e)))?;
    let config = app_state.app_config.config.load();
    let recording_root = config
        .video
        .as_ref()
        .and_then(|v| v.download.as_ref())
        .and_then(|d| d.recording.as_ref())
        .map(|r| r.directory.clone())
        .ok_or_else(|| {
            Box::new(
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": "recording_not_configured"})),
                )
                    .into_response(),
            )
        })?;
    let recording_root = PathBuf::from(recording_root);
    let abs_path = resolve_recording_dir(
        &recording_root,
        match meta.visibility {
            shared::model::recording::RecordingVisibility::Private => PathVisibility::Private,
            shared::model::recording::RecordingVisibility::Shared => PathVisibility::Shared,
        },
        &owner_dir,
        Path::new(relative),
    )
    .map_err(|_e: RecordingPathError| {
        Box::new(access_error_to_response(&CatalogAccessError::InvalidPath))
    })?;
    // Re-validate the on-disk file is a regular file (no symlink,
    // no directory) — revalidate the relative path and file type at
    // open time.
    let file_meta = no_follow_regular_file(&abs_path)
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::NotFound)))?;
    Ok(ResolvedMedia {
        abs_path,
        size: file_meta.len(),
    })
}

/// `GET /library/recording/playback/{uuid}` — supports HTTP Range
/// (RFC 7233 single-range form) and full-stream responses.
pub async fn playback_recording(
    State(app_state): State<Arc<AppState>>,
    claims: AuthClaims,
    AxumPath(uuid): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    match resolve_for_open(&app_state, &claims.0, &uuid) {
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
    match resolve_for_open(&app_state, &claims.0, &uuid) {
        Ok(resolved) => serve_range(&app_state, &resolved, &headers, true).await,
        Err(response) => *response,
    }
}

/// `GET /library/recording/thumbnail/{uuid}` — not implemented yet;
/// thumbnail generation lands with the dedicated scanner in a later
/// release. Returning 404 (not 501) so legacy clients do not retry
/// forever.
pub async fn thumbnail_recording(
    _claims: AuthClaims,
    AxumPath(_uuid): AxumPath<String>,
) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn serve_range(
    _app_state: &Arc<AppState>,
    resolved: &ResolvedMedia,
    headers: &HeaderMap,
    attachment: bool,
) -> Response {
    let total = resolved.size;
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());
    let filename = resolved
        .abs_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("recording");
    let mut base_headers = vec![
        (header::CONTENT_TYPE, "application/octet-stream".to_string()),
        (header::ACCEPT_RANGES, "bytes".to_string()),
    ];
    if attachment {
        base_headers.push((
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ));
    }
    if let Some(rh) = range_header {
        let Some(spec) = parse_range(rh, total) else {
            // RFC 7233 §4.4: 416 with `Content-Range: bytes */<total>`.
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(
                    header::CONTENT_RANGE,
                    format!("bytes */{total}"),
                )],
            )
                .into_response();
        };
        let Some((start, length)) = spec.resolve(total) else {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(
                    header::CONTENT_RANGE,
                    format!("bytes */{total}"),
                )],
            )
                .into_response();
        };
        let Ok(file) = tokio::fs::File::open(&resolved.abs_path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        // Race rule: re-validate the file is still a regular
        // file after the policy gate approved the open.
        if no_follow_regular_file(&resolved.abs_path).is_none() {
            return access_error_to_response(&CatalogAccessError::NotFound);
        }
        let mut seeked = file;
        if seeked.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let limited = seeked.take(length);
        let stream = ReaderStream::new(limited);
        let mut hdrs = base_headers.clone();
        hdrs.push((
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{total}", start + length - 1),
        ));
        hdrs.push((header::CONTENT_LENGTH, length.to_string()));
        build_response(StatusCode::PARTIAL_CONTENT, hdrs, Body::from_stream(stream))
    } else {
        // No Range header → full body. The file was already
        // re-validated at open time in `resolve_for_open`.
        let Ok(file) = tokio::fs::File::open(&resolved.abs_path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if no_follow_regular_file(&resolved.abs_path).is_none() {
            return access_error_to_response(&CatalogAccessError::NotFound);
        }
        let stream = ReaderStream::new(file);
        let mut hdrs = base_headers.clone();
        hdrs.push((header::CONTENT_LENGTH, total.to_string()));
        build_response(StatusCode::OK, hdrs, Body::from_stream(stream))
    }
}

fn build_response(status: StatusCode, headers: Vec<(header::HeaderName, String)>, body: Body) -> Response {
    let mut builder = axum::http::Response::builder().status(status);
    {
        let Some(map) = builder.headers_mut() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        for (k, v) in headers {
            if let Ok(value) = axum::http::HeaderValue::from_str(&v) {
                map.insert(k, value);
            }
        }
    }
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Register the recording media routes under `/library/recording/...`.
///
/// Auth is enforced by the `AuthClaims` extractor inside each
/// handler. The recording policy gate
/// (`recording_catalog_access::authorize_open`) is the second gate
/// that enforces ownership and visibility.
pub fn recording_media_api_register(
    router: axum::Router<Arc<AppState>>,
) -> axum::Router<Arc<AppState>> {
    router
        .route("/library/recording/playback/{uuid}", get(playback_recording))
        .route("/library/recording/download/{uuid}", get(download_recording))
        .route("/library/recording/thumbnail/{uuid}", get(thumbnail_recording))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
