//! Stages one fixture plugin with a `ui/` directory in a temporary plugins
//! directory, so the frame and rpc tests can load it through `PluginSystem`.
//!
//! The component is the `probe-bare` fixture (see `bin/build_plugin_fixture.sh`);
//! only its manifest gains a `ui:` section. A test whose fixture is not built
//! skips with a note.
#![allow(dead_code)] // each test binary uses a different subset

use crate::common::fixture_dir;
use serde_json::Map;
use sha2::Digest;
use std::{fs, path::PathBuf};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginServices, PluginSystem};

pub const ID: &str = "probe-bare";

const UI_SECTION: &str =
    "\nui:\n  locales: [en]\n  contributions:\n    - slot: menu\n      title: \"Probe\"\n      element: ui/index.html\n";

pub struct Staged {
    pub system: PluginSystem,
    /// `<plugins dir>/probe-bare`.
    pub plugin_dir: PathBuf,
    _plugins: TempDir,
    _data: TempDir,
    config: PluginsConfig,
}

impl Staged {
    /// Switch the config entry off and re-scan. The enabled load pinned the
    /// plugin first, so it is a disabled plugin and not one waiting for approval.
    pub fn disable(&self) {
        let mut config = self.config.clone();
        for entry in &mut config.plugins {
            entry.enabled = false;
        }
        self.system.reload(&config);
    }
}

/// Stage the plugin with `ui/index.html`, `ui/assets/app.js`, `ui/.secret.js` and
/// a `ui/data.xml` (a type off the allowlist). `entry` is the config entry's
/// `enabled` flag, or `None` for no entry at all. `None` when the fixture is not
/// built.
pub fn stage(entry: Option<bool>) -> Option<Staged> {
    let source = fixture_dir()?.join(ID);
    let plugins = tempfile::tempdir().expect("temporary plugins dir");
    let data = tempfile::tempdir().expect("temporary data dir");
    let plugin_dir = plugins.path().join(ID);
    fs::create_dir_all(plugin_dir.join("ui/assets")).expect("ui dir");
    fs::create_dir_all(plugin_dir.join("ui/locales")).expect("locale dir");
    fs::write(plugin_dir.join("ui/locales/en.json"), r#"{"settings.title":"Settings"}"#).expect("locale");
    fs::copy(source.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).expect("copy the component");
    let manifest = fs::read_to_string(source.join("manifest.yml")).expect("fixture manifest");
    fs::write(plugin_dir.join("manifest.yml"), format!("{manifest}{UI_SECTION}")).expect("manifest");
    fs::write(plugin_dir.join("ui/index.html"), "<!doctype html><title>probe</title>").expect("index");
    fs::write(plugin_dir.join("ui/assets/app.js"), "window.ok = true;").expect("script");
    fs::write(plugin_dir.join("ui/.secret.js"), "hidden").expect("dotfile");
    fs::write(plugin_dir.join("ui/data.xml"), "<x/>").expect("xml");

    let wasm = fs::read(source.join("plugin.wasm")).expect("read wasm");
    let sha256 = sha2::Sha256::digest(&wasm).iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    let approvals = serde_json::json!({
        ID: { "wasm_sha256": sha256, "capabilities": [] }
    });
    fs::write(data.path().join("approvals.json"), approvals.to_string()).expect("approvals");

    let entries = entry
        .map(|enabled| PluginEntry {
            id: ID.to_string(),
            enabled,
            priority: 0,
            settings: Map::new(),
            settings_version: 1,
            update_policy: shared::model::PluginUpdatePolicy::Auto,
        })
        .into_iter()
        .collect();
    let config = PluginsConfig {
        enabled: true,
        directory: plugins.path().to_string_lossy().to_string(),
        plugins: entries,
        ..PluginsConfig::default()
    };
    let services = PluginServices::new(reqwest::Client::new(), data.path().to_path_buf());
    let system = PluginSystem::load(&config, services).expect("plugin system loads").expect("plugins are enabled");
    Some(Staged { system, plugin_dir, _plugins: plugins, _data: data, config })
}
