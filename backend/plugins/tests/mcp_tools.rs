//! `PluginSystem::mcp_tools` over a loaded registry: a tool exists only for an `enabled` plugin whose export says
//! `mcp: true`, whose approval pin says `mcp: true`, and whose `input_schema` is
//! an object schema; the list follows the registry. The derivation rules
//! (naming, clashes, description) are unit-tested in `src/mcp_tools.rs`; the
//! third switch, `mcp.plugin_tools`, and the read-only rules are the endpoint's,
//! tested in `app`.
//!
//! The component is the `ui-demo` fixture, which exports `rpc` (see
//! `bin/build_plugin_fixture.sh`); only its manifest is replaced, so the tests
//! control `ui.rpc_exports`. A test whose fixture is not built skips with a note.

mod common;

use common::fixture_dir;
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use std::{fmt::Write as _, fs, path::Path, sync::Arc};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginMcpToolKind, PluginServices, PluginSystem};

const ID: &str = "ui-demo";
const SCHEMA: &str = r#"{"type":"object","properties":{"league":{"type":"string"}},"required":["league"]}"#;
const EXPORT: &str =
    "    - { name: get-fixtures, kind: read, mcp: true, description: \"Fixtures\", input_schema: rpc/get-fixtures.json }\n";

struct Staged {
    system: PluginSystem,
    config: PluginsConfig,
    plugin_dir: std::path::PathBuf,
    data: TempDir,
    _plugins: TempDir,
}

/// What a test varies: the export lines of `ui.rpc_exports`, the schema file, the
/// config entry's `enabled`, and the pin's `mcp` (`None`: no pin).
struct Setup<'a> {
    exports: &'a str,
    schema: Option<&'a str>,
    entry_enabled: bool,
    pin_mcp: Option<bool>,
}

impl Default for Setup<'_> {
    fn default() -> Self { Self { exports: EXPORT, schema: Some(SCHEMA), entry_enabled: true, pin_mcp: Some(true) } }
}

fn write_pin(data: &Path, wasm: &[u8], mcp: Option<bool>) {
    let sha256 = Sha256::digest(wasm).iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    let pins =
        mcp.map_or_else(|| json!({}), |mcp| json!({ ID: { "wasm_sha256": sha256, "capabilities": [], "mcp": mcp } }));
    fs::write(data.join("approvals.json"), pins.to_string()).expect("approvals.json");
}

fn stage(setup: &Setup<'_>) -> Option<Staged> {
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
    let manifest = format!(
        "id: {ID}\nname: UI demo\nversion: 0.3.0\napi_version: 1\nhooks: [rpc.call]\nui:\n  rpc_exports:\n{}",
        setup.exports
    );
    fs::write(plugin_dir.join("manifest.yml"), manifest).expect("manifest");
    if let Some(schema) = setup.schema {
        fs::write(plugin_dir.join("rpc/get-fixtures.json"), schema).expect("schema");
    }
    write_pin(data.path(), &wasm, setup.pin_mcp);

    let config = PluginsConfig {
        enabled: true,
        directory: plugins.path().to_string_lossy().to_string(),
        plugins: vec![PluginEntry {
            id: ID.to_string(),
            enabled: setup.entry_enabled,
            priority: 0,
            settings: Map::new(),
            settings_version: 1,
            update_policy: shared::model::PluginUpdatePolicy::Auto,
        }],
        ..PluginsConfig::default()
    };
    let services = PluginServices::new(reqwest::Client::new(), data.path().to_path_buf());
    let system = PluginSystem::load(&config, services).expect("plugin system loads").expect("plugins are enabled");
    Some(Staged { system, config, plugin_dir, data, _plugins: plugins })
}

fn names(staged: &Staged) -> Vec<String> {
    staged.system.mcp_tools(&["get_status"]).iter().map(|tool| tool.name.clone()).collect()
}

#[test]
fn an_enabled_approved_mcp_export_is_a_tool() {
    let Some(staged) = stage(&Setup::default()) else { return };
    let tools = staged.system.mcp_tools(&["get_status"]);
    assert_eq!(tools.len(), 1, "{tools:?}");
    let tool = &tools[0];
    assert_eq!(tool.name, "plugin_ui_demo_get_fixtures");
    assert_eq!((tool.plugin_id.as_str(), tool.method.as_str()), (ID, "get-fixtures"));
    assert_eq!(tool.kind, PluginMcpToolKind::Read);
    assert_eq!(tool.description, "[plugin ui-demo@0.3.0] Fixtures");
    assert_eq!(tool.input_schema["required"], json!(["league"]));
}

#[test]
fn a_write_export_is_a_write_tool() {
    let exports = "    - { name: reset, kind: write, mcp: true, input_schema: rpc/get-fixtures.json }\n";
    let Some(staged) = stage(&Setup { exports, ..Setup::default() }) else { return };
    let tools = staged.system.mcp_tools(&[]);
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0].kind, PluginMcpToolKind::Write);
}

#[test]
fn without_the_admins_approval_there_is_no_tool() {
    // A pin that says `mcp: false`, and no pin at all (the config entry's trust on
    // first use writes one without `mcp`).
    let Some(declined) = stage(&Setup { pin_mcp: Some(false), ..Setup::default() }) else { return };
    assert_eq!(names(&declined).len(), 0);
    let Some(unpinned) = stage(&Setup { pin_mcp: None, ..Setup::default() }) else { return };
    assert_eq!(names(&unpinned).len(), 0);
}

#[test]
fn a_pin_for_other_wasm_approves_nothing() {
    let Some(staged) = stage(&Setup::default()) else { return };
    assert_eq!(names(&staged).len(), 1);
    write_pin(staged.data.path(), b"some other component", Some(true));
    staged.system.reload(&staged.config);
    assert_eq!(names(&staged).len(), 0, "the pin no longer matches the wasm: not even enabled");
}

#[test]
fn a_plugin_that_is_not_enabled_has_no_tool() {
    let Some(staged) = stage(&Setup { entry_enabled: false, ..Setup::default() }) else { return };
    assert_eq!(names(&staged).len(), 0);
}

#[test]
fn an_export_without_mcp_true_is_no_tool() {
    let exports = "    - { name: get-fixtures, kind: read, input_schema: rpc/get-fixtures.json }\n";
    let Some(staged) = stage(&Setup { exports, ..Setup::default() }) else { return };
    assert_eq!(names(&staged).len(), 0);
}

#[test]
fn an_export_without_a_usable_schema_file_is_no_tool() {
    let Some(missing) = stage(&Setup { schema: None, ..Setup::default() }) else { return };
    assert_eq!(names(&missing).len(), 0);
    let Some(not_object) = stage(&Setup { schema: Some(r#"{"type":"array"}"#), ..Setup::default() }) else { return };
    assert_eq!(names(&not_object).len(), 0);
}

#[test]
fn a_built_in_name_is_never_taken() {
    let Some(staged) = stage(&Setup::default()) else { return };
    assert_eq!(staged.system.mcp_tools(&["plugin_ui_demo_get_fixtures"]).len(), 0);
}

#[test]
fn the_list_follows_the_registry() {
    let Some(staged) = stage(&Setup::default()) else { return };
    let first = staged.system.mcp_tools(&["get_status"]);
    assert_eq!(first.len(), 1);
    assert!(Arc::ptr_eq(&first, &staged.system.mcp_tools(&["get_status"])), "an unchanged registry is a cache hit");

    // The admin withdraws the MCP approval (a pin written by `PUT …/approve`), and the registry reloads.
    let wasm = fs::read(staged.plugin_dir.join("plugin.wasm")).expect("wasm");
    write_pin(staged.data.path(), &wasm, Some(false));
    staged.system.reload(&staged.config);
    assert_eq!(names(&staged).len(), 0);

    // Approved again: the tool is back.
    write_pin(staged.data.path(), &wasm, Some(true));
    staged.system.reload(&staged.config);
    assert_eq!(names(&staged), ["plugin_ui_demo_get_fixtures"]);

    // A new description in the manifest shows up after a reload too.
    let manifest = fs::read_to_string(staged.plugin_dir.join("manifest.yml")).expect("manifest");
    fs::write(staged.plugin_dir.join("manifest.yml"), manifest.replace("Fixtures", "Upcoming fixtures"))
        .expect("write");
    staged.system.reload(&staged.config);
    assert_eq!(staged.system.mcp_tools(&[])[0].description, "[plugin ui-demo@0.3.0] Upcoming fixtures");
}
