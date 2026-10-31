//! Harness shared by the fixture integration tests.
//!
//! Every test loads its probe ids from the built fixture directory with its own
//! temporary data dir, so no approval pin, KV file or log ring leaks between
//! tests. A test whose fixture is not built skips with a note (see
//! `bin/build_plugin_fixture.sh`).
#![allow(dead_code)] // each test binary uses a different subset of the harness

use serde_json::{Map, Value};
use shared::{
    model::{HookCtx, PlaylistItem, PlaylistItemHeader, PluginHook, PluginHost, PluginRunReport},
    utils::Internable,
};
use std::path::PathBuf;
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginServices, PluginSystem};

/// A loaded fixture and the data dir it persists into.
pub struct Probe {
    pub system: PluginSystem,
    pub data: TempDir,
}

/// The plugins directory of the built fixtures, or `None` (after saying so).
pub fn fixture_dir() -> Option<PathBuf> {
    let directory = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
        Into::into,
    );
    if directory.join("probe-full/plugin.wasm").is_file() {
        Some(directory)
    } else {
        eprintln!("skipped: fixture not built; run bin/build_plugin_fixture.sh");
        None
    }
}

/// A configured, approved entry (`config.plugins[]` with `enabled: true`).
pub fn probe_entry(id: &str, settings: Value) -> PluginEntry {
    let settings = match settings {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    PluginEntry { id: id.to_string(), enabled: true, priority: 0, settings }
}

/// Settings that only name the probe mode.
pub fn mode(name: &str) -> Value { serde_json::json!({ "mode": name }) }

/// The services a test gets unless it needs a custom client or clock.
pub fn default_services(data: PathBuf) -> PluginServices { PluginServices::new(reqwest::Client::new(), data) }

/// Loads `entries` from the fixture directory. `services` builds the services
/// from this test's own data dir. `None` when the fixture is not built.
pub fn load_probe(entries: Vec<PluginEntry>, services: impl FnOnce(PathBuf) -> PluginServices) -> Option<Probe> {
    let directory = fixture_dir()?;
    let data = tempfile::tempdir().expect("temporary data dir");
    let config = PluginsConfig {
        enabled: true,
        directory: directory.to_string_lossy().to_string(),
        plugins: entries,
        ..PluginsConfig::default()
    };
    let system = PluginSystem::load(&config, services(data.path().to_path_buf()))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    Some(Probe { system, data })
}

pub fn item(index: usize) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            name: format!("channel {index}").intern(),
            title: format!("channel {index}").intern(),
            group: "News".intern(),
            url: format!("http://provider/live/{index}.ts").intern(),
            input_name: "input".intern(),
            input_stream_id: index.to_string().intern(),
            chno: u32::try_from(index).unwrap_or(0),
            ..PlaylistItemHeader::default()
        },
    }
}

pub fn items(count: usize) -> Vec<PlaylistItem> { (0..count).map(item).collect() }

/// Every field a plugin could change, per item, for before/after comparison.
pub fn snapshot(items: &[PlaylistItem]) -> Vec<(String, String, String, String)> {
    items
        .iter()
        .map(|item| {
            (
                item.header.name.to_string(),
                item.header.title.to_string(),
                item.header.group.to_string(),
                item.header.url.to_string(),
            )
        })
        .collect()
}

pub fn first_title(items: &[PlaylistItem]) -> String { items[0].header.title.to_string() }

pub fn wants(system: &PluginSystem) -> bool { system.wants(PluginHook::AfterFilter, "target") }

/// One `after-filter` run. Fail-open: the call itself must never abort.
pub async fn run_stage(system: &PluginSystem, items: &mut Vec<PlaylistItem>) {
    system
        .on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), items, &mut PluginRunReport::default())
        .await
        .expect("fail-open never aborts");
}
