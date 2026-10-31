//! Guest traps and failures: every one is fail-open. The items come back
//! untouched, the call never aborts, and repeated failures trip the breaker.

mod common;

use common::{default_services, items, load_probe, mode, probe_entry, run_stage, snapshot, wants};
use serde_json::Value;
use std::time::{Duration, Instant};
use tuliprox_core::model::PluginEntry;

/// Far above the 200 ms manifest deadline: the test only proves the call ends.
const BOUND: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn runaway_loop_is_stopped_by_the_epoch_deadline() {
    // probe-timeout declares `max_exec_ms: 200`.
    let Some(probe) = load_probe(vec![probe_entry("probe-timeout", mode("loop"))], default_services) else { return };
    let mut items = items(3);
    let before = snapshot(&items);

    let started = Instant::now();
    run_stage(&probe.system, &mut items).await;

    assert!(started.elapsed() < BOUND, "the loop ran for {:?}", started.elapsed());
    assert_eq!(snapshot(&items), before, "a timed-out call changes nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_exhaustion_traps_at_the_16_mib_limit() {
    // probe-oom declares `max_memory_mb: 16`.
    let Some(probe) = load_probe(vec![probe_entry("probe-oom", mode("oom"))], default_services) else { return };
    let mut items = items(3);
    let before = snapshot(&items);

    let started = Instant::now();
    run_stage(&probe.system, &mut items).await;

    assert!(started.elapsed() < BOUND, "the allocation loop ran for {:?}", started.elapsed());
    assert_eq!(snapshot(&items), before, "an out-of-memory trap changes nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_guest_traps_and_leaves_items_alone() {
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("trap"))], default_services) else { return };
    let mut items = items(3);
    let before = snapshot(&items);

    run_stage(&probe.system, &mut items).await;

    assert_eq!(snapshot(&items), before);
    assert!(wants(&probe.system), "one trap is below the breaker threshold");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_init_error_skips_the_hook_and_leaves_items_alone() {
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("fail-init"))], default_services) else { return };
    let mut items = items(3);
    let before = snapshot(&items);

    run_stage(&probe.system, &mut items).await;

    assert_eq!(snapshot(&items), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn five_consecutive_failures_trip_the_breaker() {
    let Some(probe) = load_probe(vec![probe_entry("probe-full", mode("fail-init"))], default_services) else { return };

    for failure in 1..=4 {
        run_stage(&probe.system, &mut items(1)).await;
        assert!(wants(&probe.system), "still runnable after {failure} failure(s)");
    }
    run_stage(&probe.system, &mut items(1)).await;

    assert!(!wants(&probe.system), "the fifth consecutive failure opens the breaker");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_trapping_plugin_does_not_stop_the_next_plugin() {
    // The trapping probe runs first (lower priority value), then rename-group.
    let trap = PluginEntry { priority: -1, ..probe_entry("probe-full", mode("trap")) };
    let rename = probe_entry("rename-group", Value::Null);
    let Some(probe) = load_probe(vec![trap, rename], default_services) else { return };
    let mut items = items(2);

    run_stage(&probe.system, &mut items).await;

    assert_eq!(&*items[0].header.group, "Plugin News", "rename-group still ran");
    assert_eq!(&*items[0].header.title, "channel 0", "the trapped plugin changed no title");
}
