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
fn clean_redacted_secrets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !matches!(v, serde_json::Value::String(s) if s == shared::model::REDACTED_SECRET));
            for v in map.values_mut() {
                clean_redacted_secrets(v);
            }
        }
        serde_json::Value::Array(arr) => {
            arr.retain(|v| !matches!(v, serde_json::Value::String(s) if s == shared::model::REDACTED_SECRET));
            for v in arr.iter_mut() {
                clean_redacted_secrets(v);
            }
        }
        serde_json::Value::String(s) if s == shared::model::REDACTED_SECRET => {
            *value = serde_json::Value::Null;
        }
        _ => {}
    }
}

/// The `type: plugin` input the caller names in the payload's `input` member
/// or provides as `input_draft` (for UI form options and test queries).
///
/// A draft is an unsaved source edit, so it needs `SystemWrite` like saving it
/// would: it can name any URL and `${env:...}` reference. Stored secrets are
/// merged into a draft only while its URL is the stored input's URL, so a draft
/// can never send a saved password to another server.
#[cfg(feature = "plugins")]
fn resolve_input(
    app_state: &Arc<AppState>,
    body: &[u8],
    plugin_id: &str,
    may_write: impl FnOnce() -> bool,
) -> Result<Option<tuliprox_plugins::RpcInput>, Box<Response>> {
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(body) else { return Ok(None) };
    let Some(plugins) = app_state.plugins.as_ref() else { return Ok(None) };
    let sources = app_state.app_config.sources.load();
    let secret_paths = plugins.input_form_secret_paths(plugin_id).unwrap_or_default();

    if let Some(draft) = payload.get("input_draft").and_then(serde_json::Value::as_object) {
        if !may_write() {
            return Err(Box::new(plain_error(
                StatusCode::FORBIDDEN,
                "an `input_draft` needs the permission to edit sources",
            )));
        }
        let name = draft.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
        let url = draft.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
        let stored_input = sources
            .inputs
            .iter()
            .find(|i| i.name.as_ref() == name && i.plugin.as_deref() == Some(plugin_id) && i.url == url);

        let username = draft.get("username").and_then(serde_json::Value::as_str).map(str::to_string);
        let mut password = draft.get("password").and_then(serde_json::Value::as_str).map(str::to_string);

        // Draft secrets equal to REDACTED_SECRET are merged from the stored input of the
        // same name when it exists and is bound to the same plugin, else treated as empty.
        if password.as_deref() == Some(shared::model::REDACTED_SECRET) {
            password = stored_input.and_then(|s| s.password.clone());
        }

        let mut options_val =
            draft.get("options").cloned().unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        if let Some(stored) = stored_input {
            let stored_config = serde_json::Value::Object(
                stored.plugin_options.iter().cloned().map(|(k, v)| (k, serde_json::Value::String(v))).collect(),
            );
            tuliprox_plugins::merge_secrets(&mut options_val, &stored_config, &secret_paths);
        } else {
            clean_redacted_secrets(&mut options_val);
        }

        let url = tuliprox_core::utils::resolve_env_var(url);
        let username = username.map(|u| tuliprox_core::utils::resolve_env_var(&u));
        let password = password.map(|p| tuliprox_core::utils::resolve_env_var(&p));

        let options_map = match options_val {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let mut options = shared::model::plugin_options(&options_map);
        for (_, val) in &mut options {
            *val = tuliprox_core::utils::resolve_env_var(val);
        }

        let mut form_secrets = tuliprox_plugins::secret_values(&serde_json::Value::Object(options_map), &secret_paths);
        for s in &mut form_secrets {
            *s = tuliprox_core::utils::resolve_env_var(s);
        }

        return Ok(Some(tuliprox_plugins::RpcInput {
            plugin: plugin_id.to_string(),
            name: name.to_string(),
            url,
            username,
            password,
            options,
            form_secrets,
        }));
    }

    let Some(name) = payload.get("input").and_then(serde_json::Value::as_str) else { return Ok(None) };
    let Some(input) = sources.inputs.iter().find(|input| input.name.as_ref() == name) else { return Ok(None) };
    let Some(plugin) = input.plugin.as_deref().filter(|_| input.input_type.is_plugin()) else { return Ok(None) };

    let options_map: serde_json::Map<String, serde_json::Value> =
        input.plugin_options.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect();
    let form_secrets = tuliprox_plugins::secret_values(&serde_json::Value::Object(options_map), &secret_paths);

    Ok(Some(tuliprox_plugins::RpcInput {
        plugin: plugin.to_string(),
        name: name.to_string(),
        url: input.url.clone(),
        username: input.username.clone(),
        password: input.password.clone(),
        options: input.plugin_options.clone(),
        form_secrets,
    }))
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
    let input = match resolve_input(&app_state, &body, &id, || caller.may_write(&app_state)) {
        Ok(input) => input,
        Err(response) => return *response,
    };
    match plugins.api_rpc_call_with_input(&id, &method, &body, || caller.may_write(&app_state), input).await {
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

    /// The `ui-demo` fixture with an `input_scoped` `echo` export, its app
    /// state (sources holding `inputs`) and the open router over it. `None`
    /// when the fixtures are not built.
    #[cfg(feature = "plugins")]
    fn scoped_fixture(
        inputs: Vec<tuliprox_core::model::ConfigInput>,
    ) -> Option<(Router, tempfile::TempDir, tempfile::TempDir)> {
        use std::{sync::Arc, time::Duration};
        use tuliprox_core::model::{PluginEntry, PluginsConfig};
        use tuliprox_plugins::{PluginServices, PluginSystem};

        let fixtures = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
            std::path::PathBuf::from,
        );
        let component = fixtures.join("ui-demo").join("plugin.wasm");
        if !component.is_file() {
            eprintln!("skipped: the ui-demo fixture is not built; run bin/build_plugin_fixture.sh");
            return None;
        }
        let wasm = std::fs::read(component).expect("fixture wasm");
        let plugins = tempfile::tempdir().expect("plugins dir");
        let data = tempfile::tempdir().expect("data dir");
        let plugin_dir = plugins.path().join("ui-demo");
        std::fs::create_dir_all(plugin_dir.join("rpc")).expect("plugin dir");
        std::fs::write(plugin_dir.join("plugin.wasm"), &wasm).expect("wasm");
        std::fs::write(
            plugin_dir.join("manifest.yml"),
            r"id: ui-demo
name: UI demo
version: 0.3.0
api_version: 1
hooks: [rpc.call]
resources:
  max_exec_ms: 5000
ui:
  input_form:
    - id: main
      schema: input-schema.json
  rpc_exports:
    - { name: echo, kind: read, mcp: true, input_scoped: true, input_schema: rpc/in.json }
    - { name: options, kind: read, input_scoped: true, input_schema: rpc/in.json }
",
        )
        .expect("manifest");
        std::fs::write(
            plugin_dir.join("input-schema.json"),
            r#"{"type":"object","properties":{"api_token":{"type":"string","writeOnly":true},"library":{"type":"string","x-options-rpc":"options"}}}"#,
        )
        .expect("input schema");
        std::fs::write(plugin_dir.join("rpc/in.json"), r#"{"type":"object"}"#).expect("schema");
        let config = PluginsConfig {
            enabled: true,
            directory: plugins.path().to_string_lossy().to_string(),
            plugins: vec![PluginEntry {
                id: "ui-demo".to_string(),
                enabled: true,
                priority: 0,
                settings: serde_json::Map::new(),
                settings_version: 1,
                update_policy: shared::model::PluginUpdatePolicy::Auto,
            }],
            ..PluginsConfig::default()
        };
        let services = PluginServices::new(
            reqwest::Client::builder().timeout(Duration::from_secs(5)).build().expect("client"),
            data.path().join("plugin-data"),
        );
        let system = PluginSystem::load(&config, services).expect("load").expect("enabled");
        let app_state = crate::api::model::create_test_app_state_with_plugins(
            Config { storage_dir: data.path().to_string_lossy().to_string(), ..Config::default() },
            Some(system),
        );
        app_state.app_config.sources.store(Arc::new(crate::model::SourcesConfig {
            inputs: inputs.into_iter().map(Arc::new).collect(),
            ..crate::model::SourcesConfig::default()
        }));
        let router = plugins_rpc_api_register_unprotected().with_state(app_state);
        Some((router, plugins, data))
    }

    #[cfg(feature = "plugins")]
    fn plugin_input(name: &str, plugin: &str) -> tuliprox_core::model::ConfigInput {
        use shared::utils::Internable;
        tuliprox_core::model::ConfigInput {
            name: name.intern(),
            input_type: shared::model::InputType::Plugin,
            plugin: Some(plugin.intern()),
            url: format!("http://{name}.local/"),
            username: Some("alice".to_string()),
            password: Some("hunter2".to_string()),
            plugin_options: vec![("api_token".to_string(), "t-0k3n".to_string())],
            ..tuliprox_core::model::ConfigInput::default()
        }
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn the_rpc_route_resolves_the_input_of_a_scoped_export() {
        let Some((router, _plugins, _data)) =
            scoped_fixture(vec![plugin_input("living-room", "ui-demo"), plugin_input("bedroom", "other-plugin")])
        else {
            return;
        };
        let (status, _, body) =
            post_to(&router, "/plugins/ui-demo/rpc/echo", br#"{"input": "living-room", "message": "hi"}"#.to_vec())
                .await;
        assert_eq!(status, StatusCode::OK);
        let answer: serde_json::Value = serde_json::from_slice(&body).expect("the guest's JSON");
        assert_eq!(answer["echo"]["args"], serde_json::json!({ "message": "hi" }));
        assert_eq!(answer["echo"]["input"]["name"], "living-room");
        // The guest echoed the credentials; the route's answer carries neither.
        let text = String::from_utf8(body).expect("utf-8");
        for leaked in ["hunter2", "t-0k3n", "alice"] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn the_rpc_route_refuses_an_unknown_or_foreign_input_with_400() {
        let Some((router, _plugins, _data)) =
            scoped_fixture(vec![plugin_input("living-room", "ui-demo"), plugin_input("bedroom", "other-plugin")])
        else {
            return;
        };
        for (body, needle) in [
            (&br#"{"input": "nope"}"#[..], "no configured plugin input is named `nope`"),
            (&br#"{"input": "bedroom"}"#[..], "belongs to plugin `other-plugin`"),
            (&br"{}"[..], "needs an `input` naming a configured input"),
        ] {
            let (status, _, body) = post_to(&router, "/plugins/ui-demo/rpc/echo", body.to_vec()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
            let error: PluginRpcErrorDto = serde_json::from_slice(&body).expect("error DTO");
            assert!(error.error.contains(needle), "{} does not contain {needle}", error.error);
        }
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn the_rpc_route_accepts_input_draft_and_merges_stored_secret() {
        let Some((router, _plugins, _data)) =
            scoped_fixture(vec![plugin_input("living-room", "ui-demo"), plugin_input("bedroom", "other-plugin")])
        else {
            return;
        };
        // 1. input_draft with REDACTED_SECRET for the stored input at its stored URL:
        //    merges stored password and options secret
        let body = serde_json::json!({
            "input_draft": {
                "name": "living-room",
                "url": "http://living-room.local/",
                "password": shared::model::REDACTED_SECRET,
                "options": {
                    "api_token": shared::model::REDACTED_SECRET
                }
            },
            "message": "draft-hi"
        });
        let (status, _, res_body) =
            post_to(&router, "/plugins/ui-demo/rpc/echo", serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let answer: serde_json::Value = serde_json::from_slice(&res_body).expect("guest JSON");
        assert_eq!(answer["echo"]["args"], serde_json::json!({ "message": "draft-hi" }));
        assert_eq!(answer["echo"]["input"]["name"], "living-room");
        let merged = answer["echo"]["input"]["password"].as_str().unwrap_or_default();
        assert!(!merged.is_empty(), "the stored password was merged (and scrubbed): {answer}");
        let text = String::from_utf8(res_body).expect("utf-8");
        for leaked in ["hunter2", "t-0k3n", "alice"] {
            assert!(!text.contains(leaked), "{leaked} leaked in draft echo: {text}");
        }

        // 2. the same draft pointed at another server: the stored secrets stay home
        let moved_body = serde_json::json!({
            "input_draft": {
                "name": "living-room",
                "url": "http://attacker.example/",
                "password": shared::model::REDACTED_SECRET,
                "options": { "api_token": shared::model::REDACTED_SECRET }
            }
        });
        let (status, _, moved_res) =
            post_to(&router, "/plugins/ui-demo/rpc/echo", serde_json::to_vec(&moved_body).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let answer: serde_json::Value = serde_json::from_slice(&moved_res).expect("guest JSON");
        assert!(
            answer["echo"]["input"]["password"].is_null() || answer["echo"]["input"]["password"] == "",
            "a moved draft got the stored password: {answer}"
        );
        assert!(answer["echo"]["input"]["options"].get("api_token").is_none(), "{answer}");

        // 3. input_draft for a foreign plugin input: stored secrets must not be merged
        let foreign_body = serde_json::json!({
            "input_draft": {
                "name": "bedroom",
                "password": shared::model::REDACTED_SECRET
            },
            "message": "hi"
        });
        let (status, _, foreign_res) =
            post_to(&router, "/plugins/ui-demo/rpc/echo", serde_json::to_vec(&foreign_body).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let answer: serde_json::Value = serde_json::from_slice(&foreign_res).expect("guest JSON");
        assert!(answer["echo"]["input"]["password"].is_null() || answer["echo"]["input"]["password"] == "");
    }

    #[cfg(feature = "plugins")]
    #[tokio::test]
    async fn the_options_rpc_route_validates_shape_and_caps() {
        let Some((router, _plugins, _data)) = scoped_fixture(vec![plugin_input("living-room", "ui-demo")]) else {
            return;
        };

        // 1. Normal options call
        let body = serde_json::json!({
            "input": "living-room",
            "mode": "normal"
        });
        let (status, _, res_body) =
            post_to(&router, "/plugins/ui-demo/rpc/options", serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let options: serde_json::Value = serde_json::from_slice(&res_body).expect("options JSON");
        assert_eq!(
            options,
            serde_json::json!([
                { "value": "movies", "label": "Movies" },
                { "value": "series", "label": "TV Series" }
            ])
        );

        // 2. Oversized options (1001 items) -> 502
        let body = serde_json::json!({
            "input": "living-room",
            "mode": "oversized"
        });
        let (status, _, res_body) =
            post_to(&router, "/plugins/ui-demo/rpc/options", serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let error: PluginRpcErrorDto = serde_json::from_slice(&res_body).expect("error DTO");
        assert!(error.error.contains("exceeds 1000 items"), "unexpected error: {}", error.error);

        // 3. Wrong shape -> 502
        let body = serde_json::json!({
            "input": "living-room",
            "mode": "wrong_shape"
        });
        let (status, _, res_body) =
            post_to(&router, "/plugins/ui-demo/rpc/options", serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let error: PluginRpcErrorDto = serde_json::from_slice(&res_body).expect("error DTO");
        assert!(error.error.contains("JSON array"), "unexpected error: {}", error.error);

        // 4. Echo secret in options: secret is scrubbed
        let body = serde_json::json!({
            "input": "living-room",
            "mode": "echo_secret"
        });
        let (status, _, res_body) =
            post_to(&router, "/plugins/ui-demo/rpc/options", serde_json::to_vec(&body).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(res_body).expect("utf-8");
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("t-0k3n"));
    }

    #[cfg(feature = "plugins")]
    #[test]
    fn the_route_limit_is_the_plugin_hosts_payload_cap() {
        assert_eq!(MAX_BODY_BYTES, tuliprox_plugins::PluginSystem::MAX_RPC_PAYLOAD_BYTES);
    }
}
