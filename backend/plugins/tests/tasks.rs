//! Scheduled plugin tasks through the `task-probe` fixture: results, KV
//! between runs, `kv_generation`, the skip policy, and the states that do not
//! run.
//!
//! A test whose fixture is not built or not installed skips with a note (see
//! `bin/build_plugin_fixture.sh`: `install_manifests task-probe`).

mod common;

use common::{fixture_dir, load_probe, probe_entry, Probe};
use serde_json::json;
use shared::model::{PluginHook, PluginHost};
use std::time::Duration;
use tuliprox_plugins::{TaskOutcome, TaskTrigger};

const PLUGIN: &str = "task-probe";

fn probe(enabled: bool) -> Option<Probe> {
    if !fixture_dir()?.join(PLUGIN).join("plugin.wasm").is_file() {
        return common::skip_missing_fixture(
            "skipped: task-probe fixture not installed; run bin/build_plugin_fixture.sh",
        );
    }
    let mut entry = probe_entry(PLUGIN, json!({}));
    entry.enabled = enabled;
    load_probe(vec![entry], common::default_services)
}

fn ran(outcome: TaskOutcome) -> shared::model::PluginTaskRunDto {
    match outcome {
        TaskOutcome::Ran(run) => run,
        other => panic!("expected a run, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_run_returns_the_summary_and_kv_survives_between_runs() {
    let Some(probe) = probe(true) else { return };

    let first = ran(probe.system.run_task(PLUGIN, "ok", TaskTrigger::Manual).await);
    assert_eq!(first.summary.as_deref(), Some("ok 1"));
    assert_eq!(first.error, None);
    assert!(first.manual);

    // A new instance: only the KV file carries the count, and the host passes
    // the first run's start as `last-success-at`.
    let second = ran(probe.system.run_task(PLUGIN, "ok", TaskTrigger::Scheduled { at: 7 }).await);
    assert_eq!(second.summary, Some(format!("ok 2 last={}", first.started_at)));
    assert!(!second.manual);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_bumps_the_kv_generation_and_so_the_fingerprint() {
    let Some(probe) = probe(true) else { return };
    let before = probe.system.fingerprint(PluginHook::RunTask).expect("the plugin runs tasks");

    ran(probe.system.run_task(PLUGIN, "ok", TaskTrigger::Manual).await);
    let after = probe.system.fingerprint(PluginHook::RunTask).expect("the plugin runs tasks");
    assert_ne!(before, after, "a task may have written KV, so hook output can differ");

    // A failed run still ran the guest.
    ran(probe.system.run_task(PLUGIN, "fail", TaskTrigger::Manual).await);
    assert_ne!(after, probe.system.fingerprint(PluginHook::RunTask).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guest_err_is_a_failed_run_and_the_last_run_shows_it() {
    let Some(probe) = probe(true) else { return };

    let run = ran(probe.system.run_task(PLUGIN, "fail", TaskTrigger::Scheduled { at: 1 }).await);
    assert_eq!(run.error.as_deref(), Some("boom"));
    assert_eq!(run.summary, None);

    let tasks = probe.system.api_tasks(PLUGIN).expect("the plugin exists");
    let fail = tasks.iter().find(|task| task.id == "fail").unwrap();
    let last = fail.last_run.as_ref().expect("a failed run is the last run");
    assert_eq!(last["error"], "boom");
    let ok = tasks.iter().find(|task| task.id == "ok").unwrap();
    assert!(ok.last_run.is_none(), "a task that never ran has no last run");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_plugin_or_task_is_unknown() {
    let Some(probe) = probe(true) else { return };

    assert!(matches!(probe.system.run_task(PLUGIN, "nope", TaskTrigger::Manual).await, TaskOutcome::Unknown(_)));
    assert!(matches!(probe.system.run_task("nope", "ok", TaskTrigger::Manual).await, TaskOutcome::Unknown(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_plugin_skips_its_fires() {
    let Some(probe) = probe(false) else { return };

    assert!(matches!(
        probe.system.run_task(PLUGIN, "ok", TaskTrigger::Scheduled { at: 1 }).await,
        TaskOutcome::Skipped(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn skip_drops_a_fire_while_the_task_is_still_running() {
    let Some(probe) = probe(true) else { return };

    let slow = probe.system.run_task(PLUGIN, "slow", TaskTrigger::Scheduled { at: 1 });
    let second = async {
        // Let the first fire start and claim the task.
        tokio::time::sleep(Duration::from_millis(500)).await;
        probe.system.run_task(PLUGIN, "slow", TaskTrigger::Scheduled { at: 2 }).await
    };
    let (first, second) = tokio::join!(slow, second);

    let run = ran(first);
    assert_eq!(run.summary.as_deref(), Some("slow done"));
    assert!(matches!(second, TaskOutcome::Skipped(_)), "the second fire is dropped, not queued: {second:?}");
}
