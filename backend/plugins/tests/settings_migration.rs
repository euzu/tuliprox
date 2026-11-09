//! Settings versioning and migration over the
//! `settings-migrate-probe` fixture: its settings schema is at version 2 and its
//! `settings.migrate` turns the version 1 shape `{label, token}` into
//! `{group, retries, api_key}`.
//!
//! The app persists what `migrate_settings` returns; these tests play that part
//! by building the next config and reloading, as `plugins_settings_api` does.
//! Without the built fixture they skip (see `bin/build_plugin_fixture.sh`).

mod common;

use common::item;
use serde_json::{json, Map, Value};
use shared::model::{HookCtx, PluginHook, PluginHost, PluginRunReport, PluginStateDto};
use std::path::PathBuf;
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{MigratedSettings, PluginServices, PluginSystem};

const ID: &str = "settings-migrate-probe";

struct Env {
    system: PluginSystem,
    directory: PathBuf,
    _data: TempDir,
}

fn entry(settings: &Value, version: u32) -> PluginEntry {
    let settings: Map<String, Value> = settings.as_object().cloned().unwrap_or_default();
    PluginEntry {
        id: ID.to_string(),
        enabled: true,
        priority: 0,
        settings,
        settings_version: version,
        update_policy: shared::model::PluginUpdatePolicy::Auto,
    }
}

fn config_for(env: &Env, entry: PluginEntry) -> PluginsConfig {
    PluginsConfig {
        enabled: true,
        directory: env.directory.to_string_lossy().to_string(),
        plugins: vec![entry],
        ..PluginsConfig::default()
    }
}

/// The fixture loaded with `entry`, or `None` (after saying so) when it is not built.
fn load(settings: &Value, version: u32) -> Option<(Env, PluginsConfig)> {
    let directory = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
        Into::into,
    );
    if !directory.join(ID).join("plugin.wasm").is_file() {
        eprintln!("skipped: fixture not built; run bin/build_plugin_fixture.sh");
        return None;
    }
    let data = tempfile::tempdir().unwrap();
    let env = Env {
        // replaced below; the system needs a config that names the directory
        system: PluginSystem::load(
            &PluginsConfig {
                enabled: true,
                directory: directory.to_string_lossy().to_string(),
                ..PluginsConfig::default()
            },
            PluginServices::new(reqwest::Client::new(), data.path().to_path_buf()),
        )
        .unwrap()
        .unwrap(),
        directory,
        _data: data,
    };
    let config = config_for(&env, entry(settings, version));
    env.system.reload(&config);
    Some((env, config))
}

fn summary(env: &Env, config: &PluginsConfig) -> shared::model::PluginSummaryDto {
    env.system.api_summary(ID, Some(config)).expect("the fixture is listed")
}

/// The group and title of one `News` channel after one `after-filter` run: unchanged when the plugin did not run.
async fn run_once(env: &Env) -> (String, String) {
    let mut items = vec![item(0)];
    env.system
        .on_stage(PluginHook::AfterFilter, &HookCtx::new("target"), &mut items, &mut PluginRunReport::default())
        .await
        .expect("fail-open");
    (items[0].header.group.to_string(), items[0].header.title.to_string())
}

/// How many plugins `migrate_settings` migrated under `config`.
async fn migrated_count(env: &Env, config: &PluginsConfig) -> usize { env.system.migrate_settings(config).await.len() }

/// What the app does with a migration result: write it, reload.
fn persist(env: &Env, migrated: &MigratedSettings) -> PluginsConfig {
    let settings = Value::Object(migrated.settings.clone());
    let config = config_for(env, entry(&settings, migrated.version));
    env.system.reload(&config);
    config
}

#[tokio::test]
async fn old_settings_that_fail_the_new_schema_block_the_plugin_until_migrated() {
    let Some((env, config)) = load(&json!({"label": "Migrated", "token": "${env:PATH}"}), 1) else { return };

    // the schema moved to 2 and the v1 shape does not fit it: loaded, not running
    let blocked = summary(&env, &config);
    assert_eq!(blocked.state, PluginStateDto::Failed);
    assert!(blocked.failure.unwrap().contains("do not match the settings schema"));
    assert_eq!(blocked.settings_review, None, "a migration is still to run, so nothing to review yet");
    assert!(!env.system.wants(PluginHook::AfterFilter, "target"));
    assert_eq!(run_once(&env).await.0, "News");
    assert_eq!(env.system.settings_schema_version(ID), Some(2));
    assert!(env.system.settings_migration_pending(&config));

    // the migration step: validated, references kept, version written
    let migrated = env.system.migrate_settings(&config).await;
    assert_eq!(migrated.len(), 1);
    assert_eq!(migrated[0].id, ID);
    assert_eq!(migrated[0].version, 2);
    assert_eq!(
        Value::Object(migrated[0].settings.clone()),
        json!({"group": "Migrated", "api_key": "${env:PATH}", "retries": 3}),
        "the guest saw the resolved secret, the file keeps the reference"
    );

    let next = persist(&env, &migrated[0]);
    assert_eq!(summary(&env, &next).state, PluginStateDto::Enabled);
    assert_eq!(summary(&env, &next).settings_review, None);
    assert!(!env.system.settings_migration_pending(&next));
    assert_eq!(run_once(&env).await, ("Migrated".to_string(), "channel 0 keyed".to_string()));
}

/// The server reads `config.yml` with env resolution, so the runtime entry
/// holds the resolved secret; the references must come from the file as written
/// The migrated `api_key` must not be persisted in plain text.
#[tokio::test]
async fn a_migration_of_env_resolved_settings_keeps_the_references_as_written() {
    let path = std::env::var("PATH").expect("PATH is set");
    let Some((env, config)) = load(&json!({"label": "Migrated", "token": path}), 1) else { return };
    let written: std::collections::HashMap<String, Map<String, Value>> =
        [(ID.to_string(), json!({"label": "Migrated", "token": "${env:PATH}"}).as_object().cloned().unwrap())].into();
    let migrated = env.system.migrate_settings_as_written(&config, &written).await;
    assert_eq!(migrated.len(), 1);
    assert_eq!(
        Value::Object(migrated[0].settings.clone()),
        json!({"group": "Migrated", "api_key": "${env:PATH}", "retries": 3}),
        "the resolved secret never reaches the persisted settings"
    );
}

#[tokio::test]
async fn old_settings_that_still_fit_keep_running_while_the_migration_is_pending() {
    let Some((env, config)) = load(&json!({"group": "Old"}), 1) else { return };
    assert_eq!(summary(&env, &config).state, PluginStateDto::Enabled);
    assert_eq!(run_once(&env).await.0, "Old");
    assert!(env.system.settings_migration_pending(&config));

    let migrated = env.system.migrate_settings(&config).await;
    assert_eq!(Value::Object(migrated[0].settings.clone()), json!({"group": "Old", "retries": 3}));
    assert_eq!(migrated[0].version, 2);
}

#[tokio::test]
async fn a_failed_migration_keeps_the_settings_sets_a_review_and_is_not_retried() {
    let Some((env, config)) = load(&json!({"label": "fail"}), 1) else { return };
    assert_eq!(migrated_count(&env, &config).await, 0);

    let reviewed = summary(&env, &config);
    let review = reviewed.settings_review.expect("a failed migration is reviewed");
    assert!(review.contains("version 1") && review.contains("failed") && review.contains("kept"), "{review}");
    assert_eq!(reviewed.state, PluginStateDto::Failed, "the old settings do not fit, so it does not run");
    assert!(!env.system.settings_migration_pending(&config), "the same attempt is not repeated");
    assert_eq!(migrated_count(&env, &config).await, 0);

    // a save writes the schema version and clears the review
    let saved = env.system.api_prepare_settings(ID, &json!({"group": "Saved"}), Some(&config)).await.unwrap();
    let next = config_for(&env, entry(&Value::Object(saved.0), env.system.settings_schema_version(ID).unwrap()));
    env.system.reload(&next);
    assert_eq!(summary(&env, &next).state, PluginStateDto::Enabled);
    assert_eq!(summary(&env, &next).settings_review, None);
    assert_eq!(run_once(&env).await.0, "Saved");
}

#[tokio::test]
async fn migrated_settings_the_schema_rejects_are_not_persisted() {
    let Some((env, config)) = load(&json!({"label": "invalid"}), 1) else { return };
    assert_eq!(migrated_count(&env, &config).await, 0);
    let review = summary(&env, &config).settings_review.expect("reviewed");
    assert!(review.contains("do not match the schema") && review.contains("group"), "{review}");
    assert!(!review.contains('5'), "no value is echoed: {review}");
}

#[tokio::test]
async fn stored_settings_newer_than_the_schema_are_reviewed_not_migrated() {
    let Some((env, config)) = load(&json!({"group": "Newer"}), 3) else { return };
    let review = summary(&env, &config).settings_review.expect("a rollback is reviewed");
    assert!(review.contains("version 3") && review.contains("version 2"), "{review}");
    assert!(!env.system.settings_migration_pending(&config));
    assert_eq!(migrated_count(&env, &config).await, 0);
    // they still fit the older schema, so the plugin keeps running
    assert_eq!(summary(&env, &config).state, PluginStateDto::Enabled);
    assert_eq!(run_once(&env).await.0, "Newer");
}

#[tokio::test]
async fn settings_at_the_schema_version_need_nothing() {
    let Some((env, config)) = load(&json!({"group": "Current", "retries": 1}), 2) else { return };
    assert_eq!(summary(&env, &config).settings_review, None);
    assert!(!env.system.settings_migration_pending(&config));
    assert_eq!(migrated_count(&env, &config).await, 0);
    assert_eq!(run_once(&env).await.0, "Current");
}

#[tokio::test]
async fn a_disabled_plugin_is_not_migrated_until_it_is_enabled() {
    let Some((env, config)) = load(&json!({"label": "Later"}), 1) else { return };
    let mut disabled = config.clone();
    disabled.plugins[0].enabled = false;
    assert!(!env.system.settings_migration_pending(&disabled));
    assert_eq!(migrated_count(&env, &disabled).await, 0);
    assert!(env.system.settings_migration_pending(&config));
}
