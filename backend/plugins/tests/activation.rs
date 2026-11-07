//! The activation journal and its startup recovery, and the unattended
//! activation.
//!
//! A kill is simulated in-process: `inject_test_crash` makes the activation
//! panic right after the journal recorded a stage, which ends the blocking
//! step like a kill would (nothing after it runs). The system is then dropped
//! and `recover_activations` runs as a start would, before a fresh load.
//!
//! The replacement tests use UI-only packages (no wasm, so no fixture); the
//! `init` failure test needs the built `probe-bare` fixture and skips without it.

#![recursion_limit = "256"]

mod common;

use http::StatusCode;
use serde_json::{json, Value};
use shared::model::{PluginApproveDto, PluginOriginDto, PluginUpdatePolicy};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{
    inject_test_crash, recover_activations, ActivationKind, ActivationStage, MarketplaceExpect, PluginServices,
    PluginSystem, RecoveryOutcome,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const ID: &str = "demo";
const SOURCE: &str = "community";

struct Env {
    system: PluginSystem,
    config: PluginsConfig,
    plugins: PathBuf,
    data: PathBuf,
    _root: TempDir,
}

fn config_for(plugins: &Path, settings: Option<Value>) -> PluginsConfig {
    let mut config =
        PluginsConfig { enabled: true, directory: plugins.to_string_lossy().to_string(), ..PluginsConfig::default() };
    if let Some(Value::Object(settings)) = settings {
        config.plugins.push(PluginEntry {
            id: ID.to_string(),
            enabled: false,
            priority: 0,
            settings,
            settings_version: 1,
            update_policy: PluginUpdatePolicy::Auto,
        });
    }
    config
}

fn load(config: &PluginsConfig, data: &Path) -> PluginSystem {
    PluginSystem::load(config, PluginServices::new(reqwest::Client::new(), data.to_path_buf()))
        .expect("plugin system loads")
        .expect("plugins are enabled")
}

fn env_with(settings: Option<Value>) -> Env {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let data = root.path().join("data");
    let config = config_for(&plugins, settings);
    let system = load(&config, &data);
    Env { system, config, plugins, data, _root: root }
}

fn package(version: &str, capabilities: &[&str]) -> Vec<u8> {
    let mut text = format!("id: {ID}\nname: Demo\nversion: {version}\napi_version: 1\n");
    if !capabilities.is_empty() {
        text.push_str("capabilities:\n");
        for name in capabilities {
            text.push_str("  ");
            text.push_str(name);
            text.push_str(": true\n");
        }
    }
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file("manifest.yml", options).unwrap();
    writer.write_all(text.as_bytes()).unwrap();
    writer.start_file(format!("v{version}.txt"), options).unwrap();
    writer.write_all(version.as_bytes()).unwrap();
    writer.finish().unwrap().into_inner()
}

fn body(capabilities: &[&str]) -> PluginApproveDto {
    PluginApproveDto {
        wasm_sha256: None,
        capabilities: capabilities.iter().map(ToString::to_string).collect(),
        mcp: false,
    }
}

fn expect(version: &str) -> MarketplaceExpect {
    MarketplaceExpect {
        id: ID.to_string(),
        version: version.to_string(),
        origin: PluginOriginDto { source: SOURCE.to_string(), version: version.to_string() },
    }
}

async fn upload_and_approve(env: &Env, version: &str, capabilities: &[&str]) {
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package(version, capabilities)).await.expect("upload");
    env.system.api_approve(&guard, ID, &body(capabilities), &env.config).await.expect("approve");
}

/// Installed 1.0.0, then 2.0.0 from the marketplace: 2.0.0 runs, 1.0.0 is the prior.
async fn two_versions(env: &Env) {
    upload_and_approve(env, "1.0.0", &["storage"]).await;
    let guard = env.system.install_lock().await;
    env.system.api_upload_from_marketplace(&guard, package("2.0.0", &["storage"]), expect("2.0.0")).await.unwrap();
    env.system.api_approve(&guard, ID, &body(&["storage"]), &env.config).await.unwrap();
}

fn pin(data: &Path) -> Option<Value> {
    let text = fs::read_to_string(data.join("approvals.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.get(ID).cloned()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| entries.map(|entry| entry.unwrap().file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn installed(plugins: &Path) -> Option<String> {
    ["1.0.0", "2.0.0", "3.0.0"]
        .into_iter()
        .find(|version| plugins.join(ID).join(format!("v{version}.txt")).is_file())
        .map(String::from)
}

fn prior(data: &Path) -> Option<String> {
    let text = fs::read_to_string(data.join("plugins-prev").join(ID).join("prev.json")).ok()?;
    serde_json::from_str::<Value>(&text).ok()?.get("version")?.as_str().map(String::from)
}

/// What a fresh load after the recovery sees.
fn version_after_restart(env: &Env) -> Option<String> {
    let system = load(&env.config, &env.data);
    system.api_summary(ID, None).and_then(|summary| summary.version)
}

/// Settle with the recovery a start runs, and check it is idempotent.
fn recover(env: &Env) -> Vec<tuliprox_plugins::RecoveredActivation> {
    let found = recover_activations(&env.data, &env.plugins);
    assert!(recover_activations(&env.data, &env.plugins).is_empty(), "a second recovery finds nothing");
    assert!(names(&env.data.join("activation")).is_empty(), "no journal is left");
    assert_eq!(names(&env.plugins), [ID], "no swap leftover in the plugins dir");
    assert!(
        names(&env.data.join("plugins-prev")).iter().all(|name| !name.starts_with('.')),
        "no swap leftover in the prior dir"
    );
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_killed_at_any_stage_is_rolled_back_or_completed_at_the_next_start() {
    for stage in ActivationStage::ALL {
        let env = env_with(None);
        two_versions(&env).await;
        let pin_before = pin(&env.data);
        let guard = env.system.install_lock().await;
        env.system.api_upload(&guard, package("3.0.0", &["network"])).await.unwrap();

        inject_test_crash(&env.data, Some(stage));
        let crashed = env.system.api_approve(&guard, ID, &body(&["network"]), &env.config).await;
        inject_test_crash(&env.data, None);
        assert_eq!(crashed.unwrap_err().status, StatusCode::INTERNAL_SERVER_ERROR, "{stage}");
        drop(guard);

        let found = recover(&env);
        assert_eq!(found.len(), 1, "{stage}");
        let recovered = &found[0];
        assert_eq!(
            (recovered.id.as_str(), recovered.kind, recovered.stage),
            (ID, Some(ActivationKind::Approve), Some(stage))
        );
        assert_eq!((recovered.from.as_str(), recovered.to.as_str()), ("2.0.0", "3.0.0"));
        if stage == ActivationStage::Committed {
            assert_eq!(recovered.outcome, RecoveryOutcome::Completed, "{stage}");
            assert_eq!(installed(&env.plugins).as_deref(), Some("3.0.0"), "{stage}");
            assert_eq!(prior(&env.data).as_deref(), Some("2.0.0"), "{stage}");
            assert_eq!(pin(&env.data).unwrap()["capabilities"], json!(["network"]), "{stage}");
        } else {
            assert_eq!(recovered.outcome, RecoveryOutcome::RolledBack, "{stage}");
            assert_eq!(installed(&env.plugins).as_deref(), Some("2.0.0"), "{stage}");
            assert_eq!(prior(&env.data).as_deref(), Some("1.0.0"), "{stage}: the old prior is back");
            assert_eq!(pin(&env.data), pin_before, "{stage}: the old pin is back");
        }
        let expected = if stage == ActivationStage::Committed { "3.0.0" } else { "2.0.0" };
        assert_eq!(version_after_restart(&env).as_deref(), Some(expected), "{stage}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rollback_killed_at_any_stage_is_undone_or_completed_at_the_next_start() {
    for stage in ActivationStage::ALL {
        let env = env_with(None);
        two_versions(&env).await;
        let pin_before = pin(&env.data);
        let guard = env.system.install_lock().await;

        inject_test_crash(&env.data, Some(stage));
        let crashed = env.system.api_rollback(&guard, ID, &env.config).await;
        inject_test_crash(&env.data, None);
        assert_eq!(crashed.unwrap_err().status, StatusCode::INTERNAL_SERVER_ERROR, "{stage}");
        drop(guard);

        let found = recover(&env);
        assert_eq!(found.len(), 1, "{stage}");
        assert_eq!(found[0].kind, Some(ActivationKind::Rollback));
        if stage == ActivationStage::Committed {
            assert_eq!(found[0].outcome, RecoveryOutcome::Completed);
            assert_eq!(installed(&env.plugins).as_deref(), Some("1.0.0"), "{stage}");
            assert_eq!(prior(&env.data).as_deref(), Some("2.0.0"), "{stage}");
        } else {
            assert_eq!(found[0].outcome, RecoveryOutcome::RolledBack);
            assert_eq!(installed(&env.plugins).as_deref(), Some("2.0.0"), "{stage}");
            assert_eq!(prior(&env.data).as_deref(), Some("1.0.0"), "{stage}");
            assert_eq!(pin(&env.data), pin_before, "{stage}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_update_killed_at_any_stage_is_rolled_back_or_completed() {
    for stage in ActivationStage::ALL {
        let env = env_with(None);
        two_versions(&env).await;
        let pin_before = pin(&env.data);
        let guard = env.system.install_lock().await;
        env.system.api_upload_from_marketplace(&guard, package("3.0.0", &["storage"]), expect("3.0.0")).await.unwrap();

        inject_test_crash(&env.data, Some(stage));
        let crashed = env.system.activate_staged_unattended(&guard, ID, "3.0.0", &env.config).await;
        inject_test_crash(&env.data, None);
        assert_eq!(crashed.unwrap_err().status, StatusCode::INTERNAL_SERVER_ERROR, "{stage}");
        drop(guard);

        let found = recover(&env);
        assert_eq!(found[0].kind, Some(ActivationKind::Unattended));
        let expected = if stage == ActivationStage::Committed { "3.0.0" } else { "2.0.0" };
        assert_eq!(installed(&env.plugins).as_deref(), Some(expected), "{stage}");
        if stage != ActivationStage::Committed {
            assert_eq!(pin(&env.data), pin_before, "{stage}: the pin changes only on success");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsettled_journal_blocks_the_next_replacement_until_recovered() {
    let env = env_with(None);
    two_versions(&env).await;
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package("3.0.0", &["storage"])).await.unwrap();
    inject_test_crash(&env.data, Some(ActivationStage::Journaled));
    env.system.api_approve(&guard, ID, &body(&["storage"]), &env.config).await.unwrap_err();
    inject_test_crash(&env.data, None);

    let refused = env.system.api_approve(&guard, ID, &body(&["storage"]), &env.config).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(refused.body.error.contains("not settled"), "{}", refused.body.error);
    assert_eq!(installed(&env.plugins).as_deref(), Some("2.0.0"));
}

#[test]
fn recovery_without_journals_sweeps_install_leftovers_and_restores_an_orphaned_aside() {
    let root = tempfile::tempdir().unwrap();
    let (plugins, data) = (root.path().join("plugins"), root.path().join("data"));
    fs::create_dir_all(plugins.join(".swap~old~demo~1~1")).unwrap();
    fs::write(plugins.join(".swap~old~demo~1~1/manifest.yml"), "only copy").unwrap();
    fs::create_dir_all(plugins.join(".incoming-other-1-1")).unwrap();
    fs::create_dir_all(data.join("plugins-prev/.swap~bak~demo~1~2")).unwrap();
    fs::create_dir_all(data.join("staging/.upload-1-2-3")).unwrap();

    assert_eq!(recover_activations(&data, &plugins), Vec::new());
    assert_eq!(names(&plugins), [ID]);
    assert_eq!(fs::read_to_string(plugins.join("demo/manifest.yml")).unwrap(), "only copy");
    assert_eq!(names(&data.join("plugins-prev")), Vec::<String>::new());
    assert_eq!(names(&data.join("staging")), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_update_keeps_the_approval_and_records_the_origin() {
    let env = env_with(None);
    two_versions(&env).await;
    let guard = env.system.install_lock().await;
    env.system.api_upload_from_marketplace(&guard, package("3.0.0", &["storage"]), expect("3.0.0")).await.unwrap();

    let done = env.system.activate_staged_unattended(&guard, ID, "3.0.0", &env.config).await.unwrap();
    assert_eq!((done.from.as_str(), done.to.as_str(), done.source.as_str()), ("2.0.0", "3.0.0", SOURCE));
    assert_eq!(installed(&env.plugins).as_deref(), Some("3.0.0"));
    assert_eq!(prior(&env.data).as_deref(), Some("2.0.0"));
    let pin = pin(&env.data).unwrap();
    assert_eq!(pin["capabilities"], json!(["storage"]));
    assert_eq!(pin["origin"], json!({ "source": SOURCE, "version": "3.0.0" }));
    assert!(names(&env.data.join("activation")).is_empty(), "the journal is removed after the commit");
    assert!(!env.data.join("staging").join(ID).exists(), "the staged package was consumed");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_update_never_widens_the_approval() {
    let env = env_with(None);
    two_versions(&env).await;
    let pin_before = pin(&env.data);
    let guard = env.system.install_lock().await;
    env.system
        .api_upload_from_marketplace(&guard, package("3.0.0", &["storage", "network"]), expect("3.0.0"))
        .await
        .unwrap();

    let refused = env.system.activate_staged_unattended(&guard, ID, "3.0.0", &env.config).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(refused.body.error.contains("network"), "{}", refused.body.error);
    assert_eq!(installed(&env.plugins).as_deref(), Some("2.0.0"));
    assert_eq!(pin(&env.data), pin_before);
    assert!(env.data.join("staging").join(ID).is_dir(), "it stays staged for a review");

    // A plain upload is not a marketplace release, and a version mismatch is refused.
    env.system.api_upload(&guard, package("3.0.0", &["storage"])).await.unwrap();
    let plain = env.system.activate_staged_unattended(&guard, ID, "3.0.0", &env.config).await.unwrap_err();
    assert_eq!(plain.status, StatusCode::CONFLICT);
    let other = env.system.activate_staged_unattended(&guard, ID, "4.0.0", &env.config).await.unwrap_err();
    assert_eq!(other.status, StatusCode::CONFLICT);
    assert_eq!(pin(&env.data), pin_before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_journaled_rollback_waits_for_its_settings_and_can_be_undone() {
    let env = env_with(Some(json!({ "greeting": "v2" })));
    two_versions(&env).await;
    let pin_before = pin(&env.data);
    let guard = env.system.install_lock().await;

    let outcome = env.system.api_rollback_journaled(&guard, ID, &env.config).await.unwrap();
    assert_eq!(outcome.version, "1.0.0");
    assert!(outcome.settings.is_some());
    assert_eq!(installed(&env.plugins).as_deref(), Some("1.0.0"));
    assert_eq!(names(&env.data.join("activation")), ["demo.json"], "journaled until settled");
    // Another replacement must wait for it.
    assert_eq!(env.system.api_rollback(&guard, ID, &env.config).await.unwrap_err().status, StatusCode::CONFLICT);

    // The settings could not be saved: undone.
    let version = env.system.api_rollback_settled(&guard, ID, false, &env.config).await.unwrap();
    assert_eq!(version, "2.0.0");
    assert_eq!(installed(&env.plugins).as_deref(), Some("2.0.0"));
    assert_eq!(prior(&env.data).as_deref(), Some("1.0.0"));
    assert_eq!(pin(&env.data), pin_before);
    assert_eq!(names(&env.data.join("activation")), Vec::<String>::new());
    let nothing = env.system.api_rollback_settled(&guard, ID, true, &env.config).await.unwrap_err();
    assert_eq!(nothing.status, StatusCode::CONFLICT);

    // Saved: committed.
    env.system.api_rollback_journaled(&guard, ID, &env.config).await.unwrap();
    assert_eq!(env.system.api_rollback_settled(&guard, ID, true, &env.config).await.unwrap(), "1.0.0");
    assert_eq!(installed(&env.plugins).as_deref(), Some("1.0.0"));
    assert_eq!(prior(&env.data).as_deref(), Some("2.0.0"));
    assert_eq!(names(&env.data.join("activation")), Vec::<String>::new());
    assert_eq!(names(&env.plugins), [ID]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_journaled_rollback_killed_before_it_was_settled_comes_back_on_the_newer_version() {
    let env = env_with(Some(json!({ "greeting": "v2" })));
    two_versions(&env).await;
    let pin_before = pin(&env.data);
    let guard = env.system.install_lock().await;
    env.system.api_rollback_journaled(&guard, ID, &env.config).await.unwrap();
    drop(guard);

    let found = recover(&env);
    assert_eq!(found[0].stage, Some(ActivationStage::Checked));
    assert_eq!(found[0].outcome, RecoveryOutcome::RolledBack);
    assert_eq!(installed(&env.plugins).as_deref(), Some("2.0.0"));
    assert_eq!(prior(&env.data).as_deref(), Some("1.0.0"));
    assert_eq!(pin(&env.data), pin_before);
}

// --- with a wasm: the activation check runs `init` ---------------------------

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

/// `probe-bare` at `version`, packaged as a `.tlpx`.
fn probe_package(fixture: &Path, version: &str) -> Vec<u8> {
    let manifest = fs::read_to_string(fixture.join("manifest.yml"))
        .unwrap()
        .replace("version: 0.1.0", &format!("version: {version}"));
    let wasm = fs::read(fixture.join("plugin.wasm")).unwrap();
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    writer.start_file("manifest.yml", options).unwrap();
    writer.write_all(manifest.as_bytes()).unwrap();
    writer.start_file("plugin.wasm", options).unwrap();
    writer.write_all(&wasm).unwrap();
    writer.finish().unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unattended_update_whose_init_fails_is_rolled_back_and_the_pin_stays() {
    let Some(fixtures) = common::fixture_dir() else { return };
    let fixture = fixtures.join("probe-bare");
    if !fixture.join("plugin.wasm").is_file() {
        common::skip_missing_fixture::<()>(
            "skipped: probe-bare fixture not installed; run bin/build_plugin_fixture.sh",
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let (plugins, data) = (root.path().join("plugins"), root.path().join("data"));
    copy_dir(&fixture, &plugins.join("probe-bare"));
    // Enabled: the first load pins what is on disk, and the activation check runs `init`.
    let config = PluginsConfig {
        enabled: true,
        directory: plugins.to_string_lossy().to_string(),
        plugins: vec![common::probe_entry("probe-bare", common::mode("fail-init"))],
        ..PluginsConfig::default()
    };
    let system = load(&config, &data);
    let read_pin = || serde_json::from_str::<Value>(&fs::read_to_string(data.join("approvals.json")).unwrap()).unwrap();
    let pin_before = read_pin();

    let guard = system.install_lock().await;
    let expect = MarketplaceExpect {
        id: "probe-bare".to_string(),
        version: "0.2.0".to_string(),
        origin: PluginOriginDto { source: SOURCE.to_string(), version: "0.2.0".to_string() },
    };
    system.api_upload_from_marketplace(&guard, probe_package(&fixture, "0.2.0"), expect).await.unwrap();
    let refused = system.activate_staged_unattended(&guard, "probe-bare", "0.2.0", &config).await.unwrap_err();
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert!(refused.body.error.starts_with("rolled back to 0.1.0"), "{}", refused.body.error);
    assert_eq!(read_pin(), pin_before, "the pin changes only on success");
    assert_eq!(system.api_summary("probe-bare", None).and_then(|summary| summary.version).as_deref(), Some("0.1.0"));
    assert!(names(&data.join("activation")).is_empty(), "the undo settled the journal");
    assert_eq!(names(&plugins), ["probe-bare"]);
}
