//! `POST /api/v1/plugins/{id}/rpc/{method}`. The route sits behind `SystemRead`; a `kind: write` method also needs
//! `SystemWrite`, which the handler checks itself (`403`). Without web auth
//! every caller may write, as on every other route.
//!
//! Every non-2xx answer is a JSON `PluginRpcErrorDto` (`{error, fields?}`), also
//! the `413` of the body limit. `429` carries `Retry-After`. The limits, the
//! schema check and the guest call live in `tuliprox_plugins`
//! (`PluginSystem::api_rpc_call`), which the MCP tools call as well.
//!
//! Plugins off (`plugins.enabled: false`, or a build without the `plugins`
//! feature) answers `404`, like every other id route.

use crate::api::{
    auth_middleware::{check_permission, permission_layer, VerifiedClaims},
    model::AppState,
};
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, DefaultBodyLimit, FromRequestParts, Path, State},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Extension, Json, Router,
};
use shared::model::{Claims, Permission, PluginFieldErrorDto, PluginRpcErrorDto};
use std::{convert::Infallible, sync::Arc, time::Duration};

/// The route-level body limit: the payload cap of the rpc contract (64 KiB).
/// `PluginSystem::api_rpc_call` re-checks the length it is given.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Set on the unprotected router only: the server runs without web auth, so any
/// caller may call a `write` method. Anything that is neither this nor verified
/// claims is refused for `write`, so a missing layer fails closed.
#[derive(Debug, Clone, Copy)]
struct AuthDisabled;

/// Who is calling, as far as the `write` check needs to know.
#[cfg_attr(not(feature = "plugins"), allow(dead_code, reason = "no method is called without the plugin host"))]
enum RpcCaller {
    /// Web auth is off.
    Open,
    /// The `SystemRead` layer verified these claims.
    Claims(Box<Claims>),
    /// Neither: refused for `write` methods.
    Anonymous,
}

#[allow(clippy::unused_async_trait_impl, reason = "the trait method is async; nothing here awaits")]
impl<S: Send + Sync> FromRequestParts<S> for RpcCaller {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(VerifiedClaims(claims)) = parts.extensions.get::<VerifiedClaims>() {
            return Ok(Self::Claims(Box::new(claims.clone())));
        }
        Ok(if parts.extensions.get::<AuthDisabled>().is_some() { Self::Open } else { Self::Anonymous })
    }
}

impl RpcCaller {
    /// Whether the caller holds `SystemWrite`. A denial is audited by
    /// `check_permission`, so this is asked only for a `write` method.
    #[cfg_attr(not(feature = "plugins"), allow(dead_code, reason = "no method is called without the plugin host"))]
    fn may_write(&self, app_state: &Arc<AppState>) -> bool {
        match self {
            Self::Open => true,
            Self::Claims(claims) => {
                check_permission::<{ Permission::SystemWrite as u32 }>(app_state, claims, None).is_ok()
            }
            Self::Anonymous => false,
        }
    }
}

fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/{id}/rpc/{method}", post(post_plugin_rpc).layer(DefaultBodyLimit::max(MAX_BODY_BYTES)))
}

pub fn plugins_rpc_api_register_with_permissions(app_state: &Arc<AppState>) -> Router<Arc<AppState>> {
    routes().layer(permission_layer!(app_state, Permission::SystemRead))
}

pub fn plugins_rpc_api_register_unprotected() -> Router<Arc<AppState>> { routes().layer(Extension(AuthDisabled)) }

/// An rpc error body with `status`, and `Retry-After` when `retry_after` is set.
fn rpc_error(
    status: StatusCode,
    error: impl Into<String>,
    fields: Vec<PluginFieldErrorDto>,
    retry_after: Option<Duration>,
) -> Response {
    let mut response = (status, Json(PluginRpcErrorDto { error: error.into(), fields })).into_response();
    if let Some(wait) = retry_after {
        if let Ok(value) = header::HeaderValue::from_str(&wait.as_secs().max(1).to_string()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
    }
    response
}

fn plain_error(status: StatusCode, error: impl Into<String>) -> Response { rpc_error(status, error, Vec::new(), None) }

/// The answer for a body the extractor refused: `413` past the limit, else the
/// extractor's own status.
fn body_error(rejection: &BytesRejection) -> Response {
    let status = rejection.status();
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        plain_error(status, format!("the payload exceeds {} KiB", MAX_BODY_BYTES / 1024))
    } else {
        plain_error(status, "the request body could not be read")
    }
}

#[cfg(feature = "plugins")]
async fn post_plugin_rpc(
    State(app_state): State<Arc<AppState>>,
    Path((id, method)): Path<(String, String)>,
    caller: RpcCaller,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return body_error(&rejection),
    };
    let Some(plugins) = app_state.plugins.as_ref() else {
        return plain_error(StatusCode::NOT_FOUND, "unknown plugin");
    };
    match plugins.api_rpc_call(&id, &method, &body, || caller.may_write(&app_state)).await {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(failure) => rpc_error(failure.status, failure.error, failure.fields, failure.retry_after),
    }
}

#[cfg(not(feature = "plugins"))]
async fn post_plugin_rpc(
    State(_app_state): State<Arc<AppState>>,
    Path((_id, _method)): Path<(String, String)>,
    _caller: RpcCaller,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    match body {
        Ok(_) => plain_error(StatusCode::NOT_FOUND, "unknown plugin"),
        Err(rejection) => body_error(&rejection),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tempfile::TempDir;
    use tower::ServiceExt;

    async fn post_to(router: &Router, uri: &str, body: Vec<u8>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request = Request::builder().method("POST").uri(uri).body(Body::from(body)).unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        (status, headers, response.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    fn state(dir: &TempDir) -> Arc<AppState> {
        let config = Config { storage_dir: dir.path().to_string_lossy().to_string(), ..Config::default() };
        create_test_app_state(config)
    }

    fn open_router(dir: &TempDir) -> Router { plugins_rpc_api_register_unprotected().with_state(state(dir)) }

    #[tokio::test]
    async fn plugins_off_answers_404_with_an_error_body() {
        let dir = TempDir::new().unwrap();
        let (status, _, body) = post_to(&open_router(&dir), "/plugins/demo/rpc/ping", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let error: PluginRpcErrorDto = serde_json::from_slice(&body).unwrap();
        assert_eq!(error, PluginRpcErrorDto { error: "unknown plugin".to_string(), fields: Vec::new() });
    }

    #[tokio::test]
    async fn a_body_over_64_kib_is_a_413_with_an_error_body() {
        let dir = TempDir::new().unwrap();
        let router = open_router(&dir);
        let (status, _, body) = post_to(&router, "/plugins/demo/rpc/ping", vec![b'a'; MAX_BODY_BYTES + 1]).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(serde_json::from_slice::<PluginRpcErrorDto>(&body).is_ok(), "the 413 body is the error DTO");
        let (status, _, _) = post_to(&router, "/plugins/demo/rpc/ping", vec![b'a'; MAX_BODY_BYTES]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "exactly 64 KiB passes the limit");
    }

    #[tokio::test]
    async fn a_protected_router_refuses_a_call_without_a_valid_token() {
        let dir = TempDir::new().unwrap();
        let app_state = state(&dir);
        let router = plugins_rpc_api_register_with_permissions(&app_state).with_state(app_state);
        let (status, _, _) = post_to(&router, "/plugins/demo/rpc/ping", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let request = Request::builder()
            .method("POST")
            .uri("/plugins/demo/rpc/ping")
            .header(header::AUTHORIZATION, "Bearer not-a-token")
            .body(Body::from("{}"))
            .unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn the_plugin_routes_merge_without_overlapping() {
        // axum panics when merged routers claim the same path.
        let _ = super::super::plugins_api::plugins_api_register_unprotected()
            .merge(plugins_rpc_api_register_unprotected())
            .merge(super::super::plugins_ui_api::plugins_ui_api_register_unprotected());
    }

    #[tokio::test]
    async fn the_write_check_fails_closed() {
        let dir = TempDir::new().unwrap();
        let app_state = state(&dir);
        assert!(RpcCaller::Open.may_write(&app_state));
        assert!(!RpcCaller::Anonymous.may_write(&app_state), "no layer and no marker is not an open server");
    }

    #[cfg(feature = "plugins")]
    #[test]
    fn the_route_limit_is_the_plugin_hosts_payload_cap() {
        assert_eq!(MAX_BODY_BYTES, tuliprox_plugins::PluginSystem::MAX_RPC_PAYLOAD_BYTES);
    }
}
