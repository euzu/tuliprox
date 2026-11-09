//! The session-edge and dashboard-metrics hooks of the real `edge-probe`
//! component, through `PluginSystem`.
//!
//! The fixture is built by `bin/build_plugin_fixture.sh`, never committed. When
//! it has not been built these tests print a note and pass.

mod common;

use serde_json::{json, Value};
use shared::model::{Millis, PluginHeaderInjector, PluginUpstreamRequest};
use std::time::{Duration, Instant};
use tuliprox_core::{
    model::PluginsConfig,
    utils::{Clock, ManualClock},
};
use tuliprox_plugins::{HostClock, PluginServices, PluginSystem};

struct Edge {
    system: PluginSystem,
    clock: ManualClock,
    _data: tempfile::TempDir,
}

/// `edge-probe` configured with `settings`, on a clock the test moves.
fn load_edge(settings: Value) -> Option<Edge> {
    let directory = common::fixture_dir()?;
    if !directory.join("edge-probe/plugin.wasm").is_file() {
        return common::skip_missing_fixture("skipped: edge-probe not built; run bin/build_plugin_fixture.sh");
    }
    let data = tempfile::tempdir().expect("temporary data dir");
    let config = PluginsConfig {
        enabled: true,
        directory: directory.to_string_lossy().to_string(),
        plugins: vec![common::probe_entry("edge-probe", settings)],
        // A failing metrics call must not take the default 5 s to give up.
        max_exec: Millis::new(300),
        ..PluginsConfig::default()
    };
    let clock = ManualClock::new(1_000_000);
    let services = PluginServices::new(reqwest::Client::new(), data.path().to_path_buf())
        .with_clock(HostClock::Manual(clock.clone()));
    let system = PluginSystem::load(&config, services).expect("plugin system loads").expect("plugins are enabled");
    Some(Edge { system, clock, _data: data })
}

fn request<'a>(user: &'a str, headers: &'a [(String, String)]) -> PluginUpstreamRequest<'a> {
    PluginUpstreamRequest { target_id: 1, virtual_id: 7, url: "http://provider/live/7.ts", headers, user }
}

fn value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(header, _)| header.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn the_plugin_injects_its_headers_for_the_user() {
    let Some(edge) = load_edge(json!({})) else { return };
    assert!(edge.system.wants_upstream_headers());

    let headers = edge.system.inject_upstream_headers(&request("alice", &[])).await;
    assert_eq!(value(&headers, "X-Edge-Probe"), Some("1"));
    assert_eq!(value(&headers, "X-Edge-User"), Some("alice"));

    // The second call for the same key is served from the cache with the same answer.
    let again = edge.system.inject_upstream_headers(&request("alice", &[])).await;
    assert_eq!(headers, again);
    let other = edge.system.inject_upstream_headers(&request("bob", &[])).await;
    assert_eq!(value(&other, "X-Edge-User"), Some("bob"), "another user is another cache key");
}

#[tokio::test]
async fn a_denied_header_is_dropped_and_the_rest_is_kept() {
    let Some(edge) = load_edge(json!({ "bad_header": true })) else { return };
    let headers = edge.system.inject_upstream_headers(&request("alice", &[])).await;
    assert_eq!(value(&headers, "Host"), None);
    assert_eq!(value(&headers, "X-Edge-Probe"), Some("1"));
}

#[tokio::test]
async fn a_plugin_past_the_clamp_adds_nothing_and_the_stream_is_not_delayed() {
    let Some(edge) = load_edge(json!({ "delay_ms": 60_000 })) else { return };
    let started = Instant::now();
    let headers = edge.system.inject_upstream_headers(&request("alice", &[])).await;
    assert!(headers.is_empty(), "fail-open: nothing added");
    assert!(started.elapsed() < Duration::from_secs(2), "the 50 ms clamp held, took {:?}", started.elapsed());
}

#[tokio::test]
async fn an_err_from_the_plugin_adds_nothing() {
    let Some(edge) = load_edge(json!({ "fail": "no headers today" })) else { return };
    let headers = edge.system.inject_upstream_headers(&request("alice", &[])).await;
    assert!(headers.is_empty(), "an err adds no headers, got {headers:?}");
}

#[tokio::test]
async fn metrics_are_namespaced_and_throttled_to_one_poll_per_ten_seconds() {
    let Some(edge) = load_edge(json!({})) else { return };

    let first = edge.system.metrics_snapshot().await;
    let by_key = |metrics: &[shared::model::PluginMetricDto], key: &str| {
        metrics.iter().find(|metric| metric.key == key).map(|metric| metric.value.clone())
    };
    assert_eq!(by_key(&first, "edge-probe.answer").as_deref(), Some("42"));
    assert_eq!(by_key(&first, "edge-probe.calls").as_deref(), Some("1"));
    assert!(first.iter().all(|metric| metric.plugin == "edge-probe"));

    edge.clock.advance(9_999);
    assert_eq!(edge.system.metrics_snapshot().await, first, "a young snapshot is served as is");

    edge.clock.advance(1);
    let refreshed = edge.system.metrics_snapshot().await;
    assert_eq!(by_key(&refreshed, "edge-probe.calls").as_deref(), Some("2"), "the plugin was polled again");
    assert_eq!(edge.clock.now_ms().get(), 1_010_000);
}

#[tokio::test]
async fn a_failing_metrics_plugin_contributes_nothing_that_cycle() {
    let Some(edge) = load_edge(json!({ "delay_ms": 60_000 })) else { return };
    let metrics = edge.system.metrics_snapshot().await;
    assert!(metrics.is_empty(), "a trapping plugin adds no metrics, got {metrics:?}");
}
