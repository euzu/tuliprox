//! Each host function, observed through the probe plugin's results.
//!
//! Capability gates: kv needs `storage`, `get-config` and `now-unix` need
//! nothing, `log` needs nothing, `cache-image` needs `image_cache` and `network`.

mod common;

use common::{default_services, first_title, items, load_probe, mode, probe_entry, run_stage, wants};
use serde_json::json;
use tuliprox_core::utils::ManualClock;
use tuliprox_plugins::{HostClock, PluginServices};

#[tokio::test(flavor = "multi_thread")]
async fn kv_counter_persists_across_instances() {
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("kv"))], default_services) else { return };

    let mut first = items(1);
    run_stage(&probe.system, &mut first).await;
    assert_eq!(first_title(&first), "kv:1");

    // Each on-stage call is a fresh instance: only the KV file carries the count.
    let mut second = items(1);
    run_stage(&probe.system, &mut second).await;
    assert_eq!(first_title(&second), "kv:2");
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_refuses_a_value_over_one_mib() {
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("kv-big"))], default_services) else { return };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "kv-big:value-too-large");
}

#[tokio::test(flavor = "multi_thread")]
async fn kv_is_unavailable_without_the_storage_capability() {
    // probe-bare grants no capabilities, so `storage` is absent.
    let Some(probe) = load_probe(vec![probe_entry("probe-bare", mode("kv-denied"))], default_services) else { return };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "kv-denied:unavailable");
}

#[tokio::test(flavor = "multi_thread")]
async fn config_returns_settings_with_env_references_resolved() {
    const VAR: &str = "TULIPROX_L5_PROBE_CONFIG";
    std::env::set_var(VAR, "resolved-42");
    let settings = json!({ "mode": "config", "value": format!("${{env:{VAR}}}") });
    let Some(probe) = load_probe(vec![probe_entry("probe-full", settings)], default_services) else { return };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "config:resolved-42", "`${{env:…}}` is expanded inside the guest");
}

#[tokio::test(flavor = "multi_thread")]
async fn now_unix_is_frozen_until_the_manual_clock_moves() {
    let clock = ManualClock::new(1_700_000_000_000);
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("now"))], |data| {
        PluginServices::new(reqwest::Client::new(), data).with_clock(HostClock::Manual(clock.clone()))
    }) else {
        return;
    };

    let mut first = items(1);
    run_stage(&probe.system, &mut first).await;
    assert_eq!(first_title(&first), "now:1700000000");

    let mut again = items(1);
    run_stage(&probe.system, &mut again).await;
    assert_eq!(first_title(&again), "now:1700000000", "time does not move on its own");

    clock.advance(5_000);
    let mut later = items(1);
    run_stage(&probe.system, &mut later).await;
    assert_eq!(first_title(&later), "now:1700000005");
}

#[tokio::test(flavor = "multi_thread")]
async fn log_survives_a_burst_past_the_rate_limit() {
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("log"))], default_services) else { return };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    // The guest logs 500 lines; the excess is dropped and counted, never fatal.
    assert_eq!(first_title(&items), "log:ok");
    assert!(wants(&probe.system), "a rate-limited log call is not a failure");
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_image_is_refused_while_the_resource_cache_is_disabled() {
    let settings = json!({ "mode": "image", "url": "http://127.0.0.1:9/logo.png" });
    let Some(probe) = load_probe(vec![probe_entry("probe-full", settings)], default_services) else { return };

    let mut items = items(1);
    run_stage(&probe.system, &mut items).await;

    assert_eq!(first_title(&items), "image:err");
}
