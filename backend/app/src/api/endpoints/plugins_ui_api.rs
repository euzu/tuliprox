//! `GET /api/v1/plugins/ui`, the plugin UI frame files under
//! `/plugins/{id}/ui/{path…}` with the frame CSP, and `/plugins/{id}/ui/_sdk/bridge.js`.
//!
//! The frame routes cannot require auth: an iframe `src` carries no token. They
//! are safe because a plugin document runs in an opaque origin (the CSP carries
//! `sandbox allow-scripts`), so everything here fails closed. An unknown id, a
//! plugin that is not `enabled`, an unsafe path, a file type off the allowlist
//! and every I/O problem are a plain `404`. Only the frame routes carry the
//! frame headers; the list is an ordinary JSON route under `SystemRead`.
//!
//! The frame routes are built by [`plugins_ui_frames_register`], which mounts
//! under `web_ui_path` like the other static routes. A build without the
//! `plugins` feature has no frame routes and an empty list.

use crate::api::{auth_middleware::permission_layer, model::AppState};
#[cfg(feature = "plugins")]
use axum::{
    body::Body,
    extract::{rejection::PathRejection, Path, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use axum::{routing::get, Json, Router};
use shared::model::{Permission, PluginUiDto};
#[cfg(feature = "plugins")]
use shared::utils::concat_path_leading_slash;
use std::sync::Arc;
#[cfg(feature = "plugins")]
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::OnceLock,
};
#[cfg(feature = "plugins")]
use tuliprox_plugins::PluginSystem;

/// The frame-side client of the bridge protocol.
#[cfg(feature = "plugins")]
const BRIDGE_JS: &str = include_str!("plugin_bridge.js");

fn routes() -> Router<Arc<AppState>> { Router::new().route("/plugins/ui", get(get_plugins_ui)) }

pub fn plugins_ui_api_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    routes().layer(permission_layer!(app_state, Permission::SystemRead))
}

pub fn plugins_ui_api_register_unprotected() -> Router<Arc<AppState>> { routes() }

/// The unauthenticated frame routes, `/plugins/{id}/ui/{*path}` and the bridge
/// at `/plugins/{id}/ui/_sdk/bridge.js`, under `web_ui_path`. Frame pages
/// include the bridge as `_sdk/bridge.js`, which resolves from any `web_ui_path`.
#[cfg(feature = "plugins")]
pub fn plugins_ui_frames_register(web_ui_path: &str) -> Router<Arc<AppState>> {
    Router::new()
        .route(&concat_path_leading_slash(web_ui_path, "plugins/{id}/ui/_sdk/bridge.js"), get(get_bridge))
        .route(&concat_path_leading_slash(web_ui_path, "plugins/{id}/ui/{*path}"), get(get_frame_file))
}

#[cfg(not(feature = "plugins"))]
pub fn plugins_ui_frames_register(_web_ui_path: &str) -> Router<Arc<AppState>> { Router::new() }

#[cfg(feature = "plugins")]
async fn get_plugins_ui(State(app_state): State<Arc<AppState>>) -> Json<Vec<PluginUiDto>> {
    let Some(plugins) = app_state.plugins.clone() else {
        return Json(Vec::new());
    };
    // Reads the package's tab schema files.
    Json(tokio::task::spawn_blocking(move || plugins.api_ui()).await.unwrap_or_default())
}

#[cfg(not(feature = "plugins"))]
async fn get_plugins_ui() -> Json<Vec<PluginUiDto>> { Json(Vec::new()) }

#[cfg(feature = "plugins")]
fn not_found() -> Response { StatusCode::NOT_FOUND.into_response() }

/// Whether `If-None-Match` names `etag`: a list, `*`, or a weak validator.
#[cfg(feature = "plugins")]
fn not_modified(headers: &HeaderMap, etag: &str) -> bool {
    headers.get(header::IF_NONE_MATCH).and_then(|value| value.to_str().ok()).is_some_and(|value| {
        value
            .split(',')
            .map(str::trim)
            .any(|candidate| candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag))
    })
}

/// A `200` or `304` carrying the six frame headers; `body` is `None` for `304`.
#[cfg(feature = "plugins")]
fn frame_response(
    status: StatusCode,
    etag: &str,
    content_type: Option<&str>,
    length: Option<u64>,
    body: Body,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in PluginSystem::frame_headers(etag) {
        headers.insert(name, value);
    }
    if let Some(Ok(value)) = content_type.map(header::HeaderValue::from_str) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(length) = length {
        headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from(length));
    }
    response
}

/// The `ETag` of `bridge.js`: the crate version, a hash and the length of the
/// script that is compiled in.
#[cfg(feature = "plugins")]
fn bridge_etag() -> &'static str {
    static ETAG: OnceLock<String> = OnceLock::new();
    ETAG.get_or_init(|| {
        let mut hasher = DefaultHasher::new();
        BRIDGE_JS.hash(&mut hasher);
        format!("\"bridge-{}-{:016x}-{}\"", env!("CARGO_PKG_VERSION"), hasher.finish(), BRIDGE_JS.len())
    })
}

#[cfg(feature = "plugins")]
async fn get_bridge(headers: HeaderMap) -> Response {
    let etag = bridge_etag();
    if not_modified(&headers, etag) {
        return frame_response(StatusCode::NOT_MODIFIED, etag, None, None, Body::empty());
    }
    frame_response(
        StatusCode::OK,
        etag,
        Some("text/javascript; charset=utf-8"),
        Some(BRIDGE_JS.len() as u64),
        Body::from(BRIDGE_JS),
    )
}

#[cfg(feature = "plugins")]
async fn get_frame_file(
    State(app_state): State<Arc<AppState>>,
    path: Result<Path<(String, String)>, PathRejection>,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let Ok(Path((id, file))) = path else {
        return not_found();
    };
    // The capture is decoded once; `%2f` and `%5c` would now look like
    // separators, so the raw path is checked as it arrived.
    if !PluginSystem::ui_path_is_safe(uri.path()) {
        return not_found();
    }
    let Some(plugins) = app_state.plugins.clone() else {
        return not_found();
    };
    let asset = tokio::task::spawn_blocking(move || plugins.ui_asset(&id, &file)).await.ok().flatten();
    let Some(asset) = asset else {
        return not_found();
    };
    // The metadata of the handle that is served, not of the path that was checked.
    let Ok(file) = tokio::fs::File::open(&asset.path).await else {
        return not_found();
    };
    let Ok(meta) = file.metadata().await else {
        return not_found();
    };
    if !meta.is_file() || meta.len() > PluginSystem::MAX_UI_ASSET_BYTES {
        return not_found();
    }
    let etag = asset.etag(&meta);
    if not_modified(&headers, &etag) {
        return frame_response(StatusCode::NOT_MODIFIED, &etag, None, None, Body::empty());
    }
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    frame_response(StatusCode::OK, &etag, Some(asset.mime), Some(meta.len()), body)
}

#[cfg(all(test, feature = "plugins"))]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const HEADERS: [&str; 6] = [
        "content-security-policy",
        "x-content-type-options",
        "referrer-policy",
        "cross-origin-resource-policy",
        "cache-control",
        "etag",
    ];

    fn router(dir: &TempDir, web_ui_path: &str) -> Router {
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        plugins_ui_frames_register(web_ui_path).with_state(create_test_app_state(config))
    }

    async fn get_with(router: &Router, uri: &str, extra: &[(&str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = Request::builder().method("GET").uri(uri);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let response = router.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        (status, headers, response.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    fn assert_frame_headers(headers: &HeaderMap) {
        for name in HEADERS {
            assert!(headers.contains_key(name), "missing {name}");
        }
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert_eq!(csp, PluginSystem::FRAME_CSP);
        assert!(csp.ends_with("sandbox allow-scripts"));
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(headers["referrer-policy"], "no-referrer");
        assert_eq!(headers["cross-origin-resource-policy"], "cross-origin");
        assert_eq!(headers["cache-control"], "no-cache");
    }

    #[tokio::test]
    async fn bridge_js_is_served_with_the_script_and_all_six_headers() {
        let dir = TempDir::new().unwrap();
        let (status, headers, body) = get_with(&router(&dir, ""), "/plugins/demo/ui/_sdk/bridge.js", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_frame_headers(&headers);
        assert_eq!(headers[header::CONTENT_TYPE], "text/javascript; charset=utf-8");
        assert_eq!(body, BRIDGE_JS.as_bytes());
    }

    #[tokio::test]
    async fn bridge_js_revalidates_with_a_304_that_keeps_the_headers() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir, "");
        let (_, headers, _) = get_with(&router, "/plugins/demo/ui/_sdk/bridge.js", &[]).await;
        let etag = headers[header::ETAG].to_str().unwrap().to_string();
        for validator in [etag.clone(), format!("W/{etag}"), format!("\"other\", {etag}"), "*".to_string()] {
            let (status, headers, body) =
                get_with(&router, "/plugins/demo/ui/_sdk/bridge.js", &[("if-none-match", validator.as_str())]).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{validator}");
            assert_frame_headers(&headers);
            assert_eq!(body, Vec::<u8>::new());
        }
        let (status, _, _) =
            get_with(&router, "/plugins/demo/ui/_sdk/bridge.js", &[("if-none-match", "\"stale\"")]).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_bridge_is_mounted_under_the_web_ui_path() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir, "ui-base");
        let (status, headers, _) = get_with(&router, "/ui-base/plugins/demo/ui/_sdk/bridge.js", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_frame_headers(&headers);
        let (status, _, _) = get_with(&router, "/plugins/demo/ui/_sdk/bridge.js", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not also at the root");
        let (status, _, _) = get_with(&router, "/plugins/_sdk/bridge.js", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "the old absolute path is gone");
        let (status, _, _) = get_with(&router, "/ui-base/plugins/_sdk/bridge.js", &[]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "the old absolute path is gone");
    }

    #[tokio::test]
    async fn the_relative_bridge_include_resolves_under_a_non_root_web_ui_path() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir, "ui-base");
        // `<script src="_sdk/bridge.js">` in `/ui-base/plugins/demo/ui/menu.html`.
        let (status, headers, body) = get_with(&router, "/ui-base/plugins/demo/ui/_sdk/bridge.js", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_frame_headers(&headers);
        assert_eq!(body, BRIDGE_JS.as_bytes());
    }

    #[tokio::test]
    async fn with_plugins_off_every_frame_path_is_a_plain_404() {
        let dir = TempDir::new().unwrap();
        let router = router(&dir, "");
        for uri in [
            "/plugins/demo/ui/index.html",
            "/plugins/demo/ui/a/b.js",
            "/plugins/demo/ui/../manifest.yml",
            "/plugins/demo/ui/%2e%2e/manifest.yml",
            "/plugins/demo/ui/a%2fb.js",
            "/plugins/demo/ui/a%5cb.js",
            "/plugins/demo/ui/%00.js",
            "/plugins/demo/ui/%ff.js",
            "/plugins/_sdk/ui/index.html",
        ] {
            let (status, headers, body) = get_with(&router, uri, &[]).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
            assert_eq!(body, Vec::<u8>::new(), "{uri}");
            assert!(!headers.contains_key(header::CONTENT_SECURITY_POLICY), "a 404 is plain: {uri}");
        }
    }

    #[test]
    fn if_none_match_accepts_lists_wildcards_and_weak_validators() {
        let headers = |value: &str| {
            let mut map = HeaderMap::new();
            map.insert(header::IF_NONE_MATCH, value.parse().unwrap());
            map
        };
        assert!(not_modified(&headers("\"a\""), "\"a\""));
        assert!(not_modified(&headers("W/\"a\""), "\"a\""));
        assert!(not_modified(&headers("\"b\" , \"a\""), "\"a\""));
        assert!(not_modified(&headers("*"), "\"a\""));
        assert!(!not_modified(&headers("\"b\""), "\"a\""));
        assert!(!not_modified(&HeaderMap::new(), "\"a\""));
    }

    #[tokio::test]
    async fn the_list_is_empty_with_plugins_off() {
        let dir = TempDir::new().unwrap();
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        let router = plugins_ui_api_register_unprotected().with_state(create_test_app_state(config));
        let (status, _, body) = get_with(&router, "/plugins/ui", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(serde_json::from_slice::<Vec<PluginUiDto>>(&body).unwrap(), Vec::new());
    }
}
