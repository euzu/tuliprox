//! Incoming plugin routes are namespaced and do not inherit the management
//! router's blanket authentication: the manifest selects host or plugin auth.
use crate::api::model::AppState;
#[cfg(any(feature = "plugins", test))]
use axum::body::Body;
#[cfg(feature = "plugins")]
use axum::{body::to_bytes, http::HeaderName};
use axum::{
    extract::{OriginalUri, Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Extension, Json, Router,
};
use std::sync::Arc;

#[cfg(feature = "plugins")]
const MAX_BODY: usize = 1024 * 1024;
#[derive(Clone, Copy)]
struct WebAuthEnabled(bool);

pub fn plugins_http_api_register(web_auth_enabled: bool) -> Router<Arc<AppState>> {
    Router::new()
        .route("/plugins/{id}/http", any(handle_root))
        .route("/plugins/{id}/http/", any(handle_root))
        .route("/plugins/{id}/http/{*path}", any(handle_path))
        .layer(Extension(WebAuthEnabled(web_auth_enabled)))
}

fn error(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(serde_json::json!({"error": message}))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

async fn handle_root(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    Extension(WebAuthEnabled(auth)): Extension<WebAuthEnabled>,
    request: Request,
) -> Response {
    dispatch(state, id, uri, auth, request).await
}

async fn handle_path(
    State(state): State<Arc<AppState>>,
    Path((id, _path)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    Extension(WebAuthEnabled(auth)): Extension<WebAuthEnabled>,
    request: Request,
) -> Response {
    dispatch(state, id, uri, auth, request).await
}

async fn dispatch(state: Arc<AppState>, id: String, uri: axum::http::Uri, auth: bool, request: Request) -> Response {
    let mut response = dispatch_inner(state, id, uri, auth, request).await;
    // Plugins share the host origin. Even HTML is an inert, sandboxed document.
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'; frame-ancestors 'none'"),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(feature = "plugins")]
async fn dispatch_inner(
    state: Arc<AppState>,
    id: String,
    uri: axum::http::Uri,
    auth: bool,
    request: Request,
) -> Response {
    use crate::api::auth_middleware::{check_permission, AuthorizeRejection, AuthorizedClaims};
    use axum::extract::FromRequestParts;
    use shared::model::Permission;
    use tuliprox_plugins::{HostAuthorization, HttpRouteRequest};
    let Some(plugins) = state.plugins.as_ref() else {
        return error(StatusCode::NOT_FOUND, "unknown plugin");
    };
    let (mut parts, body) = request.into_parts();
    let authorization = if auth {
        match AuthorizedClaims::<{ Permission::SystemRead as u32 }>::from_request_parts(&mut parts, &state).await {
            Ok(AuthorizedClaims(claims)) => HostAuthorization::Authenticated {
                may_write: if matches!(parts.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
                    false
                } else {
                    check_permission::<{ Permission::SystemWrite as u32 }>(&state, &claims, None).is_ok()
                },
                username: claims.username,
            },
            Err(AuthorizeRejection::Token(crate::auth::AuthError::Forbidden)) => HostAuthorization::Forbidden,
            Err(_) => HostAuthorization::Unauthorized,
        }
    } else {
        HostAuthorization::Disabled
    };
    let prefix = format!("/plugins/{id}/http");
    let Some(path) = parts.uri.path().strip_prefix(&prefix) else {
        return error(StatusCode::NOT_FOUND, "unknown plugin route");
    };
    let path = if path.is_empty() { "/" } else { path };
    if parts.headers.len() > 64
        || parts.headers.iter().map(|(k, v)| k.as_str().len() + v.as_bytes().len()).sum::<usize>() > 16 * 1024
    {
        return error(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE, "request headers too large");
    }
    let mut headers = Vec::new();
    for (key, value) in &parts.headers {
        let Ok(value) = value.to_str() else {
            return error(StatusCode::BAD_REQUEST, "header is not valid text");
        };
        headers.push((key.to_string(), value.to_string()));
    }
    let body = match to_bytes(body, MAX_BODY).await {
        Ok(body) => body.to_vec(),
        Err(err) => {
            use std::error::Error;
            let status = if err.source().is_some_and(<dyn std::error::Error>::is::<http_body_util::LengthLimitError>) {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            return error(status, "request body could not be read within the 1 MiB limit");
        }
    };
    let is_head = parts.method == axum::http::Method::HEAD;
    let req = HttpRouteRequest {
        route: String::new(),
        method: parts.method.to_string(),
        path: path.to_string(),
        query: uri.query().unwrap_or_default().to_string(),
        params: Vec::new(),
        headers,
        body,
        principal: None,
    };
    match plugins.api_http_call(&id, req, authorization).await {
        Ok(result) => {
            let mut response = Response::new(if is_head { Body::empty() } else { Body::from(result.body) });
            *response.status_mut() = StatusCode::from_u16(result.status).unwrap_or(StatusCode::BAD_GATEWAY);
            for (name, value) in result.headers {
                if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                    response.headers_mut().append(name, value);
                }
            }
            response
        }
        Err(failure) => {
            let mut response = error(failure.status, &failure.error);
            if let Some(retry) = failure.retry_after {
                if let Ok(value) = HeaderValue::from_str(&retry.as_secs().max(1).to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
            }
            response
        }
    }
}

#[cfg(not(feature = "plugins"))]
async fn dispatch_inner(
    _state: Arc<AppState>,
    _id: String,
    _uri: axum::http::Uri,
    _auth: bool,
    _request: Request,
) -> Response {
    error(StatusCode::NOT_FOUND, "unknown plugin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::model::create_test_app_state, model::Config};
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn disabled_plugin_system_answers_404_and_inert_headers() {
        let state = create_test_app_state(Config::default());
        let router = plugins_http_api_register(true).with_state(state);
        let response = router
            .oneshot(Request::builder().uri("/plugins/demo/http/api/channels/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("sandbox"));
    }

    #[cfg(feature = "plugins")]
    fn fixture() -> Option<(tempfile::TempDir, tuliprox_plugins::PluginSystem)> {
        use sha2::Digest;
        use std::{fs, path::PathBuf};
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/plugin-fixtures/build/wasm32-wasip2/release/plugin_fixture_http_probe.wasm");
        if !source.is_file() {
            assert!(std::env::var("CI").as_deref() != Ok("true"), "build http-probe fixture first");
            eprintln!("skipped: build http-probe fixture first");
            return None;
        }
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plugins/http-probe");
        fs::create_dir_all(&dir).unwrap();
        let wasm = fs::read(source).unwrap();
        fs::write(dir.join("plugin.wasm"), &wasm).unwrap();
        fs::write(
            dir.join("manifest.yml"),
            include_str!("../../../../../plugins/plugin-api/fixtures/http-probe/manifest.yml"),
        )
        .unwrap();
        let hash = sha2::Sha256::digest(&wasm).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let data = root.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(
            data.join("approvals.json"),
            serde_json::json!({"http-probe":{"wasm_sha256":hash,"capabilities":["storage","http_routes"]}}).to_string(),
        )
        .unwrap();
        let config = tuliprox_core::model::PluginsConfig {
            enabled: true,
            directory: root.path().join("plugins").to_string_lossy().into(),
            plugins: vec![tuliprox_core::model::PluginEntry {
                id: "http-probe".into(),
                enabled: true,
                priority: 0,
                settings: Default::default(),
                settings_version: 1,
                update_policy: shared::model::PluginUpdatePolicy::Auto,
            }],
            ..Default::default()
        };
        let system = tuliprox_plugins::PluginSystem::load(
            &config,
            tuliprox_plugins::PluginServices::new(reqwest::Client::new(), data),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            system.api_summary("http-probe", Some(&config)).unwrap().state,
            shared::model::PluginStateDto::Enabled
        );
        Some((root, system))
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn real_routes_enforce_auth_and_preserve_paths_bodies_and_statuses() {
        use crate::{api::model::create_test_app_state_with_plugins, auth::create_jwt_web_user};
        use shared::model::{Permission, UserId, WebAuthConfigDto, WebUiConfigDto};
        let Some((_root, system)) = fixture() else { return };
        let web = WebUiConfigDto {
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "test".into(),
                secret: "fixture-secret".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = Config { web_ui: Some((&web).into()), ..Default::default() };
        let auth = config.web_ui.as_ref().unwrap().auth.as_ref().unwrap();
        let reader =
            create_jwt_web_user(auth, "reader", Permission::SystemRead.into(), 0, UserId::web("reader")).unwrap();
        let unrelated =
            create_jwt_web_user(auth, "other", Permission::ConfigRead.into(), 0, UserId::web("other")).unwrap();
        let state = create_test_app_state_with_plugins(config, Some(system));
        // Use the full API composition with a web UI prefix, not just this module.
        let router = super::super::v1_api::v1_api_register(true, &state, "/control").with_state(state);
        let url = "/control/api/v1/plugins/http-probe/http/host/a%2Fb/?x=a%2Fb";
        for (token, method, status) in [
            (None, "GET", StatusCode::UNAUTHORIZED),
            (Some("invalid"), "GET", StatusCode::UNAUTHORIZED),
            (Some(unrelated.as_str()), "GET", StatusCode::FORBIDDEN),
            (Some(reader.as_str()), "PATCH", StatusCode::FORBIDDEN),
            (Some(reader.as_str()), "GET", StatusCode::CREATED),
        ] {
            let mut req = Request::builder().method(method).uri(url);
            if let Some(token) = token {
                req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            let response = router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), status, "{method}, token={}", token.is_some());
            if status == StatusCode::CREATED {
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["params"], serde_json::json!([["id", "a%2Fb"]]));
                assert_eq!(body["principal"], "reader");
                assert_eq!(body["query"], "x=a%2Fb");
            }
        }
        for suffix in ["", "/"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(format!("/control/api/v1/plugins/http-probe/http{suffix}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "namespace root must be routed");
        }
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control/api/v1/plugins/http-probe/http/custom/")
                    .header("x-api-key", "fixture-key")
                    .body(Body::from(vec![0, 255]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "custom auth must bypass host JWT requirements");
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["body"], serde_json::json!([0, 255]));
        assert_eq!(body["principal"], "external-user");
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control/api/v1/plugins/http-probe/http/custom/")
                    .header("x-api-key", "fixture-key")
                    .body(Body::from(vec![0; MAX_BODY + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
