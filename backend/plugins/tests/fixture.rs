//! Loads the real fixture component and runs it through `PluginSystem`.
//!
//! The fixture is built by `bin/build_plugin_fixture.sh`, never committed. When
//! it has not been built these tests print a note and pass, so a plain
//! `cargo test` works on a machine without the `wasm32-wasip2` target.

mod common;

use serde_json::Map;
use shared::{
    model::{HookCtx, PlaylistItem, PlaylistItemHeader, PluginHook, PluginHost, PluginRunReport},
    utils::Internable,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginServices, PluginSystem};

fn fixture_dir() -> PathBuf {
    std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
        Into::into,
    )
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

/// The loaded fixture, or `None` (after saying so) when it is not built.
fn load_fixture() -> Option<PluginSystem> {
    let directory = fixture_dir();
    if !directory.join("rename-group/plugin.wasm").is_file() {
        return common::skip_missing_fixture("skipped: fixture not built; run bin/build_plugin_fixture.sh");
    }
    // Approval is by config entry: rename-group runs only when listed.
    let entry = PluginEntry { id: "rename-group".to_string(), enabled: true, priority: 0, settings: Map::new() };
    let config = PluginsConfig {
        enabled: true,
        directory: directory.to_string_lossy().to_string(),
        plugins: vec![entry],
        ..PluginsConfig::default()
    };
    // Each test gets its own data dir: the approval pins are written there.
    let unique = NEXT.fetch_add(1, Ordering::SeqCst);
    let data = std::env::temp_dir().join(format!("tuliprox-plugin-fixture-{}-{unique}", std::process::id()));
    let services = PluginServices::new(reqwest::Client::new(), data);
    Some(PluginSystem::load(&config, services).expect("load").expect("plugins are enabled"))
}

fn item(index: usize, group: &str) -> PlaylistItem {
    PlaylistItem {
        header: PlaylistItemHeader {
            name: format!("channel {index}").intern(),
            title: format!("channel {index}").intern(),
            group: group.intern(),
            url: format!("http://provider/live/{index}.ts").intern(),
            input_name: "input".intern(),
            input_stream_id: index.to_string().intern(),
            chno: u32::try_from(index).unwrap_or(0),
            ..PlaylistItemHeader::default()
        },
    }
}

fn groups(items: &[PlaylistItem]) -> Vec<String> { items.iter().map(|item| item.header.group.to_string()).collect() }

#[test]
fn fixture_loads_and_subscribes_to_after_filter() {
    let Some(system) = load_fixture() else { return };
    // Every probe id under the same directory is discovered too; only rename-group is approved here.
    assert!(system.plugin_ids().iter().any(|id| id == "rename-group"));
    assert!(system.wants(PluginHook::AfterFilter, "any-target"));
}

#[tokio::test]
async fn fixture_renames_news_and_leaves_everything_else_alone() {
    let Some(system) = load_fixture() else { return };
    let mut items = vec![item(0, "News"), item(1, "Sport"), item(2, "News"), item(3, "Movies")];
    let before: Vec<_> = items.iter().map(|item| (item.header.name.clone(), item.header.url.clone())).collect();

    system
        .on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), &mut items, &mut PluginRunReport::default())
        .await
        .expect("fail-open never aborts");

    assert_eq!(groups(&items), ["Plugin News", "Sport", "Plugin News", "Movies"]);
    let after: Vec<_> = items.iter().map(|item| (item.header.name.clone(), item.header.url.clone())).collect();
    assert_eq!(before, after, "only the group changes");
    // host-owned fields survive the round trip
    assert_eq!(&*items[2].header.input_stream_id, "2");
    assert_eq!(&*items[2].header.input_name, "input");
    assert_eq!(items[2].header.chno, 2);
}

#[tokio::test]
async fn handles_stay_correct_across_batches() {
    let Some(system) = load_fixture() else { return };
    // More items than one batch, so the second batch's handles restart at 0.
    let mut items: Vec<_> =
        (0..10_000).map(|index| item(index, if index % 3 == 0 { "News" } else { "Other" })).collect();

    let mut report = PluginRunReport::default();
    system.on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), &mut items, &mut report).await.expect("ok");

    assert_eq!(items.len(), 10_000);
    let stats = &report.hooks;
    assert_eq!(stats.len(), 1, "one entry per (plugin, hook)");
    assert_eq!((stats[0].kept, stats[0].dropped, stats[0].modified, stats[0].errors), (10_000, 0, 3334, 0));
    assert_eq!(report.fingerprints.len(), 1, "the plugin that ran is fingerprinted");
    for (index, item) in items.iter().enumerate() {
        let expected = if index % 3 == 0 { "Plugin News" } else { "Other" };
        assert_eq!(&*item.header.group, expected, "item {index}");
    }
}

#[tokio::test]
async fn an_empty_list_is_a_no_op() {
    let Some(system) = load_fixture() else { return };
    let mut items = Vec::new();
    system
        .on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), &mut items, &mut PluginRunReport::default())
        .await
        .expect("ok");
    assert!(items.is_empty());
}

#[tokio::test]
async fn clones_share_the_loaded_plugins() {
    let Some(system) = load_fixture() else { return };
    let clone = system.clone();
    drop(system);
    let mut items = vec![item(0, "News")];
    clone
        .on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), &mut items, &mut PluginRunReport::default())
        .await
        .expect("ok");
    assert_eq!(groups(&items), ["Plugin News"]);
}
