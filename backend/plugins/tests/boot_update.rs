//! The background boot update: which plugins it looks at, the budget, the
//! report, and the task drain.
//!
//! No network beyond a local listener that never answers. The drain test
//! needs the built `task-probe` fixture and skips without it.

#![recursion_limit = "256"]

mod common;

use shared::model::{
    PluginApproveDto, PluginBootUpdateOutcomeDto, PluginOriginDto, PluginUpdateEvent, PluginUpdatePolicy,
};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginRepository, PluginsConfig};
use tuliprox_plugins::{
    read_boot_report, record_recovered, run_boot_update, ActivationKind, ActivationStage, FetchPolicy,
    MarketplaceExpect, PluginServices, PluginSystem, RecoveredActivation, RecoveryOutcome, TaskTrigger,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const ID: &str = "demo";
const SOURCE: &str = "community";

struct Env {
    system: PluginSystem,
    config: PluginsConfig,
    data: PathBuf,
    _root: TempDir,
}

fn package(version: &str) -> Vec<u8> {
    let text = format!("id: {ID}\nname: Demo\nversion: {version}\napi_version: 1\n");
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file("manifest.yml", options).unwrap();
    writer.write_all(text.as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

fn entry(enabled: bool, policy: PluginUpdatePolicy) -> PluginEntry {
    PluginEntry {
        id: ID.to_string(),
        enabled,
        priority: 0,
        settings: serde_json::Map::new(),
        settings_version: 1,
        update_policy: policy,
    }
}

/// `demo` 1.0.0 installed from `community` (so its pin has an origin), with
/// the source at `url` and the boot update on under `budget_ms`.
async fn env(entry: PluginEntry, url: &str, budget_ms: u64) -> Env {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let data = root.path().join("data");
    let config = PluginsConfig {
        enabled: true,
        directory: plugins.to_string_lossy().to_string(),
        auto_update_on_boot: true,
        boot_update_budget: shared::model::Millis::new(budget_ms),
        repositories: vec![PluginRepository {
            id: SOURCE.to_string(),
            name: SOURCE.to_string(),
            url: url.to_string(),
            enabled: true,
        }],
        plugins: vec![entry],
        ..PluginsConfig::default()
    };
    let system = PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.clone()))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    let guard = system.install_lock().await;
    let expect = MarketplaceExpect {
        id: ID.to_string(),
        version: "1.0.0".to_string(),
        origin: PluginOriginDto { source: SOURCE.to_string(), version: "1.0.0".to_string() },
    };
    system.api_upload_from_marketplace(&guard, package("1.0.0"), expect).await.unwrap();
    let body = PluginApproveDto { wasm_sha256: None, capabilities: Vec::new(), mcp: false };
    system.api_approve(&guard, ID, &body, &config).await.unwrap();
    drop(guard);
    Env { system, config, data, _root: root }
}

/// A listener that accepts and never answers, so a sync hangs until a deadline.
async fn silent_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            open.push(socket);
        }
    });
    format!("http://{address}/marketplace.json")
}

fn report_file(data: &Path) -> PathBuf { tuliprox_plugins::boot_report_path(data) }

// Real time with a short budget: paused time (`start_paused`) needs tokio's
// `test-util` feature, which this crate's dev-dependencies do not enable.
#[tokio::test(flavor = "multi_thread")]
async fn a_source_that_never_answers_is_cut_at_the_budget_and_reported() {
    let url = silent_server().await;
    let env = env(entry(true, PluginUpdatePolicy::Auto), &url, 300).await;
    let started = std::time::Instant::now();
    let events = std::sync::Mutex::new(Vec::<PluginUpdateEvent>::new());

    let report =
        run_boot_update(&env.system, &env.config, &reqwest::Client::new(), FetchPolicy { allow_http: true }, |event| {
            events.lock().unwrap().push(event);
        })
        .await;

    assert!(started.elapsed() < Duration::from_secs(20), "cut at the budget, not at the 30 s catalog deadline");
    assert!(report.budget_exceeded);
    assert_eq!(report.budget_ms, 300);
    assert!(report.started_at.is_some() && report.finished_at.is_some());
    assert_eq!(report.entries.len(), 1);
    let only = &report.entries[0];
    assert_eq!((only.id.as_str(), only.from.as_str(), only.to.as_deref()), (ID, "1.0.0", None));
    assert_eq!(only.outcome, PluginBootUpdateOutcomeDto::Failed);
    assert!(only.reason.as_deref().unwrap_or_default().contains("budget"), "{:?}", only.reason);
    assert!(events.lock().unwrap().is_empty(), "no release is known, so there is no event");
    assert_eq!(read_boot_report(&env.data).await, report, "the report is stored");
    // Still the installed package.
    assert_eq!(env.system.api_summary(ID, None).and_then(|summary| summary.version).as_deref(), Some("1.0.0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_whose_policy_or_switch_keeps_them_are_skipped_without_network() {
    for (entry, reason) in [
        (entry(true, PluginUpdatePolicy::Manual), "update policy `manual`"),
        (entry(true, PluginUpdatePolicy::Pinned), "update policy `pinned`"),
        (entry(false, PluginUpdatePolicy::Auto), "not enabled"),
    ] {
        // An unroutable url: a sync would fail, but none happens.
        let env = env(entry, "http://192.0.2.1/marketplace.json", 30_000).await;
        let report = run_boot_update(&env.system, &env.config, &reqwest::Client::new(), FetchPolicy::default(), |_| {
            panic!("no event for a skipped plugin")
        })
        .await;
        assert!(!report.budget_exceeded);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].outcome, PluginBootUpdateOutcomeDto::Skipped);
        assert_eq!(report.entries[0].reason.as_deref(), Some(reason));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_boot_update_does_nothing_and_keeps_the_recovered_ids() {
    let mut env = env(entry(true, PluginUpdatePolicy::Auto), "http://192.0.2.1/marketplace.json", 30_000).await;
    record_recovered(
        &env.data,
        &[RecoveredActivation {
            id: ID.to_string(),
            kind: Some(ActivationKind::Unattended),
            from: "1.0.0".to_string(),
            to: "2.0.0".to_string(),
            stage: Some(ActivationStage::Entered),
            outcome: RecoveryOutcome::RolledBack,
        }],
    );
    env.config.auto_update_on_boot = false;
    let report = run_boot_update(&env.system, &env.config, &reqwest::Client::new(), FetchPolicy::default(), |_| {
        panic!("no event when the boot update is off")
    })
    .await;
    assert!(report.started_at.is_none(), "nothing ran");
    assert_eq!(report.entries, Vec::new());
    assert_eq!(report.recovered, [ID]);
    assert_eq!(env.system.boot_report().await, report);

    // A start that recovered nothing clears the list.
    record_recovered(&env.data, &[]);
    assert_eq!(read_boot_report(&env.data).await.recovered, Vec::<String>::new());
    assert!(report_file(&env.data).is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_drain_wait_times_out_while_a_task_runs_and_succeeds_after_it() {
    const PLUGIN: &str = "task-probe";
    let Some(fixtures) = common::fixture_dir() else { return };
    if !fixtures.join(PLUGIN).join("plugin.wasm").is_file() {
        common::skip_missing_fixture::<()>(
            "skipped: task-probe fixture not installed; run bin/build_plugin_fixture.sh",
        );
        return;
    }
    let Some(probe) =
        common::load_probe(vec![common::probe_entry(PLUGIN, serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    assert_eq!(probe.system.running_task_count(), 0);
    assert!(probe.system.wait_for_idle_tasks(Duration::from_millis(10)).await, "idle at once");

    let system = probe.system.clone();
    let slow = tokio::spawn(async move { system.run_task(PLUGIN, "slow", TaskTrigger::Manual).await });
    // Let the run start.
    for _ in 0..100 {
        if probe.system.running_task_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(probe.system.running_task_count(), 1);
    assert!(!probe.system.wait_for_idle_tasks(Duration::from_millis(50)).await, "a running task holds the drain");

    assert!(probe.system.wait_for_idle_tasks(Duration::from_secs(60)).await);
    slow.await.unwrap();
    assert_eq!(probe.system.running_task_count(), 0);
}
