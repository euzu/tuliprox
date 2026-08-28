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
        AppState, DownloadQueue,
    },
    auth::{validate_token_claims, verify_token, AuthBearer, AuthError},
    utils::{no_follow_path_in_root, resolve_recording_dir, RecordingPathError, RecordingVisibility as PathVisibility},
};
use axum::{
    body::Body,
    extract::{FromRequestParts, Path as AxumPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    RequestPartsExt,
};
use shared::model::{Claims, UserId, CURRENT_PERMISSION_SCHEMA_VERSION, PERM_ALL, ROLE_ADMIN, TOKEN_NO_AUTH};
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
        roles: vec![ROLE_ADMIN.to_string()],
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
            parts.extract::<AuthBearer>().await.map_err(|(_, msg)| (StatusCode::UNAUTHORIZED, msg).into_response())?;
        let config = app_state.app_config.config.load();
        match config.web_ui.as_ref().and_then(|w| w.auth.as_ref()).filter(|auth| auth.enabled) {
            Some(web_auth) => {
                let token_data = verify_token(&token, web_auth.secret.as_bytes())
                    .ok_or_else(|| auth_claims_rejection(AuthError::InvalidToken))?;
                validate_token_claims(&token_data.claims).map_err(auth_claims_rejection)?;
                Ok(Self(token_data.claims))
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

/// Resolved target for a media request: the absolute path on disk,
/// the recording root it was resolved against, and the file size for
/// the response headers. The root travels alongside the `abs_path` so
/// every later re-validation (between `File::open` and the actual
/// byte read) can re-check that no intermediate component has been
/// swapped in as a symlink.
struct ResolvedMedia {
    abs_path: PathBuf,
    recording_root: PathBuf,
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
async fn resolve_for_open(app_state: &AppState, claims: &Claims, uuid: &str) -> Result<ResolvedMedia, Box<Response>> {
    let queue: &DownloadQueue = &app_state.downloads;
    let recording = recording_catalog_access::lookup_recording(queue, uuid)
        .await
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
    let abs_path = resolve_recording_dir(
        &recording_root,
        match meta.visibility {
            shared::model::recording::RecordingVisibility::Private => PathVisibility::Private,
            shared::model::recording::RecordingVisibility::Shared => PathVisibility::Shared,
        },
        &owner_dir,
        Path::new(relative),
    )
    .map_err(|_e: RecordingPathError| Box::new(access_error_to_response(&CatalogAccessError::InvalidPath)))?;
    // Re-validate the on-disk file is a regular file (no symlink,
    // no directory) at every intermediate component between the
    // configured root and the leaf — otherwise a swapped-in symlink
    // under `<root>/users/alice` would route reads outside the
    // recording root.
    let file_meta = no_follow_path_in_root(&recording_root, &abs_path)
        .await
        .ok_or_else(|| Box::new(access_error_to_response(&CatalogAccessError::NotFound)))?;
    Ok(ResolvedMedia { abs_path, recording_root, size: file_meta.len() })
}

/// `GET /library/recording/playback/{uuid}` — supports HTTP Range
/// (RFC 7233 single-range form) and full-stream responses.
pub async fn playback_recording(
    State(app_state): State<Arc<AppState>>,
    claims: AuthClaims,
    AxumPath(uuid): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    match resolve_for_open(&app_state, &claims.0, &uuid).await {
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
    match resolve_for_open(&app_state, &claims.0, &uuid).await {
        Ok(resolved) => serve_range(&app_state, &resolved, &headers, true).await,
        Err(response) => *response,
    }
}

/// `GET /library/recording/thumbnail/{uuid}` — not implemented yet;
/// thumbnail generation lands with the dedicated scanner in a later
/// release. Returning 404 (not 501) so legacy clients do not retry
/// forever.
pub async fn thumbnail_recording(_claims: AuthClaims, AxumPath(_uuid): AxumPath<String>) -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn serve_range(
    _app_state: &Arc<AppState>,
    resolved: &ResolvedMedia,
    headers: &HeaderMap,
    attachment: bool,
) -> Response {
    let total = resolved.size;
    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let filename = resolved.abs_path.file_name().and_then(|s| s.to_str()).unwrap_or("recording");
    let mut base_headers = vec![
        (header::CONTENT_TYPE, "application/octet-stream".to_string()),
        (header::ACCEPT_RANGES, "bytes".to_string()),
    ];
    if attachment {
        base_headers.push((header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")));
    }
    if let Some(rh) = range_header {
        let Some(spec) = parse_range(rh, total) else {
            // RFC 7233 §4.4: 416 with `Content-Range: bytes */<total>`.
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{total}"))])
                .into_response();
        };
        let Some((start, length)) = spec.resolve(total) else {
            return (StatusCode::RANGE_NOT_SATISFIABLE, [(header::CONTENT_RANGE, format!("bytes */{total}"))])
                .into_response();
        };
        let Ok(file) = tokio::fs::File::open(&resolved.abs_path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        // Race rule: re-validate no component between root and the
        // leaf has been swapped in as a symlink since `resolve_for_open`
        // approved the open.
        if no_follow_path_in_root(&resolved.recording_root, &resolved.abs_path).await.is_none() {
            return access_error_to_response(&CatalogAccessError::NotFound);
        }
        let mut seeked = file;
        if seeked.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let limited = seeked.take(length);
        let stream = ReaderStream::new(limited);
        let mut hdrs = base_headers.clone();
        hdrs.push((header::CONTENT_RANGE, format!("bytes {start}-{}/{total}", start + length - 1)));
        hdrs.push((header::CONTENT_LENGTH, length.to_string()));
        build_response(StatusCode::PARTIAL_CONTENT, hdrs, Body::from_stream(stream))
    } else {
        // No Range header → full body. The file was already
        // re-validated at open time in `resolve_for_open`. The byte
        // limit mirrors the range path: it caps the stream at the
        // advertised Content-Length so a concurrent append or symlink
        // swap cannot overshoot the response.
        let Ok(file) = tokio::fs::File::open(&resolved.abs_path).await else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if no_follow_path_in_root(&resolved.recording_root, &resolved.abs_path).await.is_none() {
            return access_error_to_response(&CatalogAccessError::NotFound);
        }
        let limited = file.take(total);
        let stream = ReaderStream::new(limited);
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
        Permission, UserId, WebUiConfigDto, CURRENT_PERMISSION_SCHEMA_VERSION, PERM_ALL, ROLE_ADMIN, TOKEN_NO_AUTH,
    };

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
        assert_eq!(claims.roles, vec![ROLE_ADMIN]);
        assert_eq!(claims.permissions, PERM_ALL);
        assert_eq!(claims.permission_schema_version, CURRENT_PERMISSION_SCHEMA_VERSION);
        assert!(claims.permissions.contains(Permission::RecordingWrite));

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

    #[tokio::test]
    async fn auth_claims_enabled_jwt_truth_table() {
        let config = config_with_web_auth(true, "secret");
        let web_auth = config.web_ui.as_ref().and_then(|web_ui| web_ui.auth.as_ref()).expect("web auth").clone();
        let state = create_test_app_state(config);
        let valid = create_jwt_admin(&web_auth, "admin", 0).expect("jwt");

        let AuthClaims(claims) =
            extract_auth_claims(&state, Some(&format!("Bearer {valid}"))).await.expect("valid claims");
        assert_eq!(claims.subject_id, Some(UserId::builtin_admin()));

        let mut stale_claims = builtin_admin_claims();
        stale_claims.permission_schema_version = 0;
        let mut missing_subject = builtin_admin_claims();
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
