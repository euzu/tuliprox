//! Pin integrity over the real `rename-group`
//! component: a `plugin.wasm` change outside approve, update and rollback means
//! `PendingApproval`, and the swapped bytes never run, whether the registry
//! learns of it from the directory watcher, from a reload, or at the next start.
//!
//! The fixture is copied into a temporary plugins directory per test, because
//! the tests overwrite its `plugin.wasm`. Without the built fixture they skip
//! (see `bin/build_plugin_fixture.sh`).

mod common;

use common::item;
use shared::model::{HookCtx, PluginHook, PluginHost, PluginRunReport, PluginStateDto};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginServices, PluginSystem};

const ID: &str = "rename-group";

struct Env {
    system: PluginSystem,
    config: PluginsConfig,
    wasm: PathBuf,
    data: PathBuf,
    _root: TempDir,
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

/// The fixture copied into a temporary plugins directory, approved by an
/// enabled config entry (trust on first use pins it), and running. `None`
/// (after saying so) when the fixture is not built.
fn env() -> Option<Env> {
    let fixtures = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
        Into::into,
    );
    let source = fixtures.join(ID);
    if !source.join("plugin.wasm").is_file() {
        eprintln!("skipped: fixture not built; run bin/build_plugin_fixture.sh");
        return None;
    }
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    copy_dir(&source, &plugins.join(ID));
    let data = root.path().join("data");
    let entry = PluginEntry {
        id: ID.to_string(),
        enabled: true,
        priority: 0,
        settings: serde_json::Map::new(),
        settings_version: 1,
        update_policy: shared::model::PluginUpdatePolicy::Auto,
    };
    let config = PluginsConfig {
        enabled: true,
        directory: plugins.to_string_lossy().to_string(),
        plugins: vec![entry],
        ..PluginsConfig::default()
    };
    let system = PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.clone()))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    let wasm = plugins.join(ID).join("plugin.wasm");
    Some(Env { system, config, wasm, data, _root: root })
}

fn state(env: &Env) -> PluginStateDto { env.system.api_summary(ID, Some(&env.config)).expect("listed").state }

/// What the plugin does to the group `News`: `Plugin News` while it runs.
async fn group_after_a_run(system: &PluginSystem) -> String {
    let mut items = vec![item(0)];
    let mut report = PluginRunReport::default();
    system
        .on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), &mut items, &mut report)
        .await
        .expect("fail-open");
    items[0].header.group.to_string()
}

/// The same component with one more (empty) custom section: a different hash
/// that is still a valid component, so only the pin can keep it from running.
fn swapped(original: &[u8]) -> Vec<u8> {
    let mut bytes = original.to_vec();
    bytes.extend_from_slice(&[0x00, 0x03, 0x01, b'x', 0x00]);
    bytes
}

fn pins(env: &Env) -> Vec<u8> { fs::read(env.data.join("approvals.json")).expect("the first load pinned the plugin") }

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tokio::test]
async fn a_swapped_wasm_stops_running_at_the_next_reload_and_is_never_pinned_again() {
    let Some(env) = env() else { return };
    assert_eq!(state(&env), PluginStateDto::Enabled);
    assert_eq!(group_after_a_run(&env.system).await, "Plugin News");
    let pinned = pins(&env);

    fs::write(&env.wasm, swapped(&fs::read(&env.wasm).unwrap())).unwrap();
    // No watcher event is needed: a reload re-reads and re-hashes every package.
    env.system.reload(&env.config);

    let summary = env.system.api_summary(ID, Some(&env.config)).unwrap();
    assert_eq!(summary.state, PluginStateDto::PendingApproval);
    assert!(summary.failure.unwrap().contains("plugin.wasm changed"));
    assert!(!env.system.wants(PluginHook::AfterFilter, "target"));
    assert_eq!(group_after_a_run(&env.system).await, "News", "the swapped bytes never run");
    assert_eq!(pins(&env), pinned, "a reload never writes the new hash as approved");
}

#[tokio::test]
async fn a_manual_swap_while_the_server_runs_is_caught_by_the_watcher() {
    let Some(env) = env() else { return };
    assert_eq!(group_after_a_run(&env.system).await, "Plugin News");
    let pinned = pins(&env);

    fs::write(&env.wasm, swapped(&fs::read(&env.wasm).unwrap())).unwrap();
    wait_for("the watcher to rescan the swapped plugin.wasm", || state(&env) == PluginStateDto::PendingApproval);

    assert_eq!(group_after_a_run(&env.system).await, "News", "the running plugin stopped with the rescan");
    assert_eq!(pins(&env), pinned);
}

#[tokio::test]
async fn the_approved_bytes_run_again_when_they_return() {
    let Some(env) = env() else { return };
    let original = fs::read(&env.wasm).unwrap();
    fs::write(&env.wasm, swapped(&original)).unwrap();
    env.system.reload(&env.config);
    assert_eq!(state(&env), PluginStateDto::PendingApproval);

    fs::write(&env.wasm, &original).unwrap();
    env.system.reload(&env.config);
    assert_eq!(state(&env), PluginStateDto::Enabled);
    assert_eq!(group_after_a_run(&env.system).await, "Plugin News");
}

#[tokio::test]
async fn a_restart_over_a_swapped_wasm_stays_pending() {
    let Some(env) = env() else { return };
    fs::write(&env.wasm, swapped(&fs::read(&env.wasm).unwrap())).unwrap();
    // A second system over the same plugins and data dirs: what a restart sees.
    let restarted = PluginSystem::load(&env.config, PluginServices::new(reqwest::Client::new(), env.data.clone()))
        .unwrap()
        .unwrap();
    let summary = restarted.api_summary(ID, Some(&env.config)).unwrap();
    assert_eq!(summary.state, PluginStateDto::PendingApproval);
    assert_eq!(group_after_a_run(&restarted).await, "News");
}
