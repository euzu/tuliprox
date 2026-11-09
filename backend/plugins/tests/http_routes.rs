//! Real wasm route handler + authentication, built by `build_plugin_fixture.sh`.
mod common;
use http::StatusCode;
use serde_json::{json, Value};
use sha2::Digest;
use std::{fmt::Write, fs, path::PathBuf};
use tempfile::TempDir;
use tuliprox_core::model::PluginsConfig;
use tuliprox_plugins::{HostAuthorization, HttpRouteRequest, PluginServices, PluginSystem};

struct Staged {
    system: PluginSystem,
    config: PluginsConfig,
    _dir: TempDir,
    _data: TempDir,
}
fn stage() -> Option<Staged> {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/plugin-fixtures/build/wasm32-wasip2/release/plugin_fixture_http_probe.wasm");
    if !source.is_file() {
        return common::skip_missing_fixture("build http-probe with bin/build_plugin_fixture.sh");
    }
    let dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("http-probe");
    fs::create_dir(&plugin).unwrap();
    let wasm = fs::read(source).unwrap();
    fs::write(plugin.join("plugin.wasm"), &wasm).unwrap();
    fs::write(
        plugin.join("manifest.yml"),
        include_str!("../../../plugins/plugin-api/fixtures/http-probe/manifest.yml"),
    )
    .unwrap();
    let hash = sha2::Sha256::digest(&wasm).iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    fs::write(
        data.path().join("approvals.json"),
        json!({"http-probe":{"wasm_sha256":hash,"capabilities":["storage","http_routes"]}}).to_string(),
    )
    .unwrap();
    let config = PluginsConfig {
        enabled: true,
        directory: dir.path().to_string_lossy().into(),
        plugins: vec![common::probe_entry("http-probe", json!({}))],
        ..PluginsConfig::default()
    };
    let system =
        PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.path().into())).unwrap().unwrap();
    assert_eq!(system.api_summary("http-probe", Some(&config)).unwrap().state, shared::model::PluginStateDto::Enabled);
    Some(Staged { system, config, _dir: dir, _data: data })
}
fn request(method: &str, path: &str) -> HttpRouteRequest {
    HttpRouteRequest {
        route: "spoofed".into(),
        method: method.into(),
        path: path.into(),
        query: "q=a%2Fb".into(),
        headers: vec![],
        params: vec![],
        body: vec![0, 255, 42],
        principal: Some("spoofed-admin".into()),
    }
}
async fn status(system: &PluginSystem, req: HttpRouteRequest, auth: HostAuthorization) -> StatusCode {
    system.api_http_call("http-probe", req, auth).await.map_or_else(|e| e.status, |_| StatusCode::OK)
}
#[tokio::test]
async fn authenticated_host_routes_enforce_permissions_and_pass_trusted_identity() {
    let Some(staged) = stage() else { return };
    let system = &staged.system;
    assert_eq!(
        status(system, request("GET", "/host/42/"), HostAuthorization::Unauthorized).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            system,
            request("PATCH", "/host/42/"),
            HostAuthorization::Authenticated { username: "reader".into(), may_write: false }
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let mut req = request("PATCH", "/host/42/");
    req.headers.push(("authorization".into(), "Bearer private-host-token".into()));
    let response = system
        .api_http_call(
            "http-probe",
            req,
            HostAuthorization::Authenticated { username: "admin".into(), may_write: true },
        )
        .await
        .unwrap();
    assert_eq!(response.status, 201);
    assert!(response.headers.iter().all(|(key, _)| key == "content-type"));
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["calls"], 1, "refused requests never reach handler");
    assert_eq!(body["principal"], "admin");
    assert_eq!(body["route"], "host-write");
    assert_eq!(body["params"], json!([["id", "42"]]));
    assert_eq!(body["body"], json!([0, 255, 42]));
    assert_eq!(body["headers"], json!([]));
    assert_eq!(body["query"], "q=a%2Fb");
}
#[tokio::test]
async fn plugin_auth_remains_required_when_web_auth_is_off_and_errors_fail_closed() {
    let Some(staged) = stage() else { return };
    let system = &staged.system;
    assert_eq!(
        status(system, request("POST", "/custom/"), HostAuthorization::Disabled).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            system,
            request("POST", "/custom/"),
            HostAuthorization::Authenticated { username: "admin".into(), may_write: true }
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let mut req = request("POST", "/custom/");
    req.query = "auth-error".into();
    let failure = system.api_http_call("http-probe", req, HostAuthorization::Disabled).await.unwrap_err();
    assert_eq!(failure.status, StatusCode::BAD_GATEWAY);
    assert!(!failure.error.contains("private-auth-secret"));
    let mut req = request("POST", "/custom/");
    req.headers.push(("x-api-key".into(), "fixture-key".into()));
    let response = system.api_http_call("http-probe", req, HostAuthorization::Unauthorized).await.unwrap();
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["principal"], "external-user");
    assert_eq!(body["calls"], 1);
    // Explicitly permitted login path: a plugin can issue its own tokens.
    let response =
        system.api_http_call("http-probe", request("POST", "/login/"), HostAuthorization::Unauthorized).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&response.body).unwrap()["principal"], "anonymous-login");
}
#[tokio::test]
async fn unknown_disabled_and_bad_handlers_never_fall_through() {
    let Some(staged) = stage() else { return };
    let system = &staged.system;
    assert_eq!(status(system, request("GET", "/missing/"), HostAuthorization::Disabled).await, StatusCode::NOT_FOUND);
    assert_eq!(
        status(system, request("DELETE", "/host/42/"), HostAuthorization::Disabled).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    for query in ["trap", "bad-status"] {
        let mut req = request("GET", "/host/42/");
        req.query = query.into();
        assert_eq!(status(system, req, HostAuthorization::Disabled).await, StatusCode::BAD_GATEWAY);
    }
    let mut config = staged.config.clone();
    config.plugins[0].enabled = false;
    system.reload(&config);
    assert_eq!(status(system, request("GET", "/host/42/"), HostAuthorization::Disabled).await, StatusCode::CONFLICT);
}
