//! `PluginSystem::api_rpc_call_with_input`: an `input_scoped` export over a real
//! guest. The component is the `ui-demo` fixture (`echo` returns the payload it
//! was given), so these tests check what the guest actually reads and what
//! reaches the caller back. The refusal cases and the payload shaping are
//! unit-tested in `src/rpc.rs`; a test whose fixture is not built skips with a
//! note.

mod common;

use common::fixture_dir;
use http::StatusCode;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, fs, path::Path};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginServices, PluginSystem, RpcInput};

const ID: &str = "ui-demo";
const INPUT_SCHEMA: &str =
    r#"{"type":"object","properties":{"message":{"type":"string"}},"additionalProperties":false}"#;
const EXPORTS: &str = "    - { name: echo, kind: read, mcp: true, input_scoped: true, input_schema: rpc/echo.json }\n";

struct Staged {
    system: PluginSystem,
    data: TempDir,
    _plugins: TempDir,
}

fn write_pin(data: &Path, wasm: &[u8]) {
    let sha256 = Sha256::digest(wasm).iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    let pins = json!({ ID: { "wasm_sha256": sha256, "capabilities": [], "mcp": true } });
    fs::write(data.join("approvals.json"), pins.to_string()).expect("approvals.json");
}

/// The `ui-demo` component with `EXPORTS`, approved for `mcp`.
fn stage() -> Option<Staged> {
    let component = fixture_dir()?.join(ID).join("plugin.wasm");
    if !component.is_file() {
        return common::skip_missing_fixture(&format!(
            "skipped: the {ID} fixture is not built; run bin/build_plugin_fixture.sh"
        ));
    }
    let wasm = fs::read(component).expect("component");
    let plugins = tempfile::tempdir().expect("temporary plugins dir");
    let data = tempfile::tempdir().expect("temporary data dir");
    let plugin_dir = plugins.path().join(ID);
    fs::create_dir_all(plugin_dir.join("rpc")).expect("plugin dir");
    fs::write(plugin_dir.join("plugin.wasm"), &wasm).expect("wasm");
    fs::write(
        plugin_dir.join("manifest.yml"),
        format!(
            "id: {ID}\nname: UI demo\nversion: 0.3.0\napi_version: 1\nhooks: [rpc.call]\nui:\n  rpc_exports:\n{EXPORTS}"
        ),
    )
    .expect("manifest");
    fs::write(plugin_dir.join("rpc/echo.json"), INPUT_SCHEMA).expect("schema");
    write_pin(data.path(), &wasm);

    let config = PluginsConfig {
        enabled: true,
        directory: plugins.path().to_string_lossy().to_string(),
        plugins: vec![PluginEntry {
            id: ID.to_string(),
            enabled: true,
            priority: 0,
            settings: Map::new(),
            settings_version: 1,
            update_policy: shared::model::PluginUpdatePolicy::Auto,
        }],
        ..PluginsConfig::default()
    };
    let services = PluginServices::new(reqwest::Client::new(), data.path().to_path_buf());
    let system = PluginSystem::load(&config, services).expect("plugin system loads").expect("plugins are enabled");
    Some(Staged { system, data, _plugins: plugins })
}

/// How many guest calls ran to an outcome (rpc calls are counted here and never
/// on the circuit breaker), which a refused call must leave untouched.
fn guest_calls(system: &PluginSystem) -> u64 { system.api_stats(ID).expect("the plugin is loaded").calls }

fn input() -> RpcInput {
    RpcInput {
        plugin: ID.to_string(),
        name: "living-room".to_string(),
        url: "http://living-room.local/".to_string(),
        username: Some("alice".to_string()),
        password: Some("hunter2".to_string()),
        options: vec![("region".to_string(), "eu".to_string()), ("api_token".to_string(), "t-0k3n".to_string())],
        form_secrets: Vec::new(),
    }
}

#[tokio::test]
async fn a_scoped_call_hands_the_guest_the_arguments_and_the_input_context() {
    let Some(staged) = stage() else { return };
    let answer = staged
        .system
        .api_rpc_call_with_input(ID, "echo", br#"{"input": "living-room", "message": "hi"}"#, || false, Some(input()))
        .await
        .expect("call");
    let answer: Value = serde_json::from_str(&answer).expect("the guest's JSON");
    assert_eq!(answer["echo"]["args"], json!({ "message": "hi" }));
    let context = &answer["echo"]["input"];
    assert_eq!(context["name"], "living-room");
    assert_eq!(context["url"], "http://living-room.local/");
    assert_eq!(context["options"]["region"], "eu");
    // The guest echoed the credentials back; the host scrubbed them from the answer.
    assert_eq!(context["username"], shared::model::REDACTED_SECRET);
    assert_eq!(context["password"], shared::model::REDACTED_SECRET);
    assert_eq!(context["options"]["api_token"], shared::model::REDACTED_SECRET);
    for leaked in ["hunter2", "t-0k3n", "alice"] {
        assert!(!answer.to_string().contains(leaked), "{leaked} leaked: {answer}");
    }
    assert_eq!(guest_calls(&staged.system), 1);
}

#[tokio::test]
async fn a_scoped_export_called_without_the_input_context_is_refused_before_the_guest() {
    let Some(staged) = stage() else { return };
    // The plain entry point has no input to resolve: the call never reaches the guest.
    let refused = staged
        .system
        .api_rpc_call(ID, "echo", br#"{"input": "living-room", "message": "hi"}"#, || false)
        .await
        .expect_err("refused");
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(refused.error.contains("no configured plugin input is named `living-room`"), "{}", refused.error);
    assert_eq!(guest_calls(&staged.system), 0, "a refused call never reaches the guest");
    assert!(staged.data.path().join("approvals.json").is_file(), "the approval pin survived the refusal");

    // With the input it does reach the guest, so the refusal above was the input.
    assert!(staged
        .system
        .api_rpc_call_with_input(ID, "echo", br#"{"input": "living-room"}"#, || false, Some(input()))
        .await
        .is_ok());
    assert_eq!(guest_calls(&staged.system), 1);
}

#[tokio::test]
async fn a_foreign_input_is_refused_and_the_guest_is_not_called() {
    let Some(staged) = stage() else { return };
    let foreign = RpcInput { plugin: "other-plugin".to_string(), ..input() };
    let refused = staged
        .system
        .api_rpc_call_with_input(ID, "echo", br#"{"input": "living-room"}"#, || false, Some(foreign))
        .await
        .expect_err("refused");
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(refused.error.contains("belongs to plugin `other-plugin`"), "{}", refused.error);
    assert_eq!(guest_calls(&staged.system), 0);
}

#[tokio::test]
async fn an_evil_scoped_payload_cannot_widen_the_guest_call() {
    let Some(staged) = stage() else { return };
    // A payload that is not an object, an `input` that is not a name, an
    // argument that fails the input schema, and an input name that is not the
    // resolved one all stop at the host.
    let cases: [(&[u8], StatusCode); 4] = [
        (b"[1,2,3]", StatusCode::BAD_REQUEST),
        (br#"{"input": 5}"#, StatusCode::BAD_REQUEST),
        (br#"{"input": "living-room", "message": 5}"#, StatusCode::UNPROCESSABLE_ENTITY),
        (br#"{"input": "bedroom"}"#, StatusCode::BAD_REQUEST),
    ];
    for (body, expected) in cases {
        let refused = staged
            .system
            .api_rpc_call_with_input(ID, "echo", body, || false, Some(input()))
            .await
            .expect_err("refused");
        assert_eq!(refused.status, expected, "{body:?}: {refused:?}");
    }
    assert_eq!(guest_calls(&staged.system), 0, "the guest is not asked about a refused payload");
    // The plugin is still healthy: a valid call goes through afterwards.
    let answer = staged
        .system
        .api_rpc_call_with_input(ID, "echo", br#"{"input": "living-room"}"#, || false, Some(input()))
        .await
        .expect("the guest is still healthy");
    let answer: Value = serde_json::from_str(&answer).expect("JSON");
    assert_eq!(answer["echo"]["args"], json!({}));
    assert_eq!(guest_calls(&staged.system), 1);
}
