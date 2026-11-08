//! Real wasm reconciliation: create/update/delete, failure fallback and durable ownership.
mod common;
use common::probe_entry;
use serde_json::{json, Value};
use sha2::Digest;
use shared::model::{
    ManagedChannelRequest, ManagedChannelStage, ManagedDestination, PlaylistEntry, PluginHost, PluginRunReport,
    PluginStateDto,
};
use std::{fmt::Write, fs, path::PathBuf};
use tempfile::TempDir;
use tuliprox_core::model::PluginsConfig;
use tuliprox_plugins::{PluginServices, PluginSystem};

struct Staged {
    system: PluginSystem,
    config: PluginsConfig,
    root: TempDir,
    data: TempDir,
}
fn stage(owners: &[&str]) -> Option<Staged> {
    let wasm = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/plugin-fixtures/build/wasm32-wasip2/release/plugin_fixture_managed_probe.wasm");
    if !wasm.is_file() {
        return common::skip_missing_fixture("build managed-probe fixture with bin/build_plugin_fixture.sh");
    }
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let wasm = fs::read(wasm).unwrap();
    let hash = sha2::Sha256::digest(&wasm).iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    let mut approvals = serde_json::Map::new();
    for owner in owners {
        let dir = root.path().join(owner);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("plugin.wasm"), &wasm).unwrap();
        let manifest = include_str!("../../../plugins/plugin-api/fixtures/managed-probe/manifest.yml")
            .replace("id: managed-probe", &format!("id: {owner}"));
        fs::write(dir.join("manifest.yml"), manifest).unwrap();
        approvals.insert(owner.to_string(), json!({"wasm_sha256":hash,"capabilities":["storage","playlist_write"]}));
    }
    fs::write(data.path().join("approvals.json"), Value::Object(approvals).to_string()).unwrap();
    let config = PluginsConfig {
        enabled: true,
        directory: root.path().to_string_lossy().into(),
        plugins: owners.iter().map(|owner| probe_entry(owner, json!({}))).collect(),
        ..Default::default()
    };
    let system =
        PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.path().into())).unwrap().unwrap();
    for owner in owners {
        assert_eq!(system.api_summary(owner, Some(&config)).unwrap().state, PluginStateDto::Enabled);
    }
    Some(Staged { system, config, root, data })
}
async fn set(system: &PluginSystem, owner: &str, value: Value) {
    system.api_rpc_call(owner, "set", value.to_string().as_bytes(), || true).await.unwrap();
}
async fn channels(system: &PluginSystem, input: &str) -> Vec<shared::model::PlaylistItem> {
    let inputs = vec![input.to_string()];
    let request =
        ManagedChannelRequest { destination: ManagedDestination::Input, name: input, stage: None, inputs: &inputs };
    let mut result = Vec::new();
    for owner in system.plugin_ids() {
        result.extend(system.managed_channels(&owner, &request, &mut PluginRunReport::default()).await.unwrap());
    }
    result
}

async fn target_channels(
    system: &PluginSystem,
    name: &str,
    stage: ManagedChannelStage,
    inputs: &[&str],
) -> Vec<shared::model::PlaylistItem> {
    let inputs = inputs.iter().map(ToString::to_string).collect::<Vec<_>>();
    let request =
        ManagedChannelRequest { destination: ManagedDestination::Target, name, stage: Some(stage), inputs: &inputs };
    system.managed_channels("managed-probe", &request, &mut PluginRunReport::default()).await.unwrap()
}

#[tokio::test]
async fn reconcile_adds_updates_and_deletes_without_changing_identity() {
    let Some(s) = stage(&["managed-probe"]) else { return };
    let first = channels(&s.system, "provider").await;
    assert_eq!(first.len(), 2);
    assert_eq!(&*first[0].header.input_name, "provider");
    assert_eq!(first[0].header.id, first[0].header.input_stream_id);
    assert_ne!(&*first[0].header.input_stream_id, "provider-id-spoof");
    set(&s.system, "managed-probe", json!({"count":1,"variant":"new","label":"Renamed"})).await;
    let updated = channels(&s.system, "provider").await;
    assert_eq!(updated.len(), 1);
    assert_eq!(first[0].get_uuid(), updated[0].get_uuid());
    assert_ne!(first[0].header.url, updated[0].header.url);
    assert_eq!(&*updated[0].header.name, "Renamed 0");
    set(&s.system, "managed-probe", json!({"destination":"provider"})).await;
    assert!(channels(&s.system, "unselected-input").await.is_empty());
    set(&s.system, "managed-probe", json!({"mode":"empty"})).await;
    assert!(channels(&s.system, "provider").await.is_empty());
    set(&s.system, "managed-probe", json!({"mode":"fail"})).await;
    assert!(channels(&s.system, "provider").await.is_empty(), "empty success must replace the previous nonempty cache");
}

#[tokio::test]
async fn errors_traps_timeouts_and_invalid_snapshots_keep_last_good_channels() {
    let Some(s) = stage(&["managed-probe"]) else { return };
    let first = channels(&s.system, "provider").await;
    for state in [json!({"mode":"fail"}), json!({"mode":"trap"}), json!({"mode":"spin"}), json!({"duplicate":true})] {
        set(&s.system, "managed-probe", state).await;
        let fallback = channels(&s.system, "provider").await;
        assert_eq!(
            fallback.iter().map(PlaylistEntry::get_uuid).collect::<Vec<_>>(),
            first.iter().map(PlaylistEntry::get_uuid).collect::<Vec<_>>()
        );
    }
    // Restart with the last guest state still invalid: fallback must come from disk.
    let restarted = PluginSystem::load(&s.config, PluginServices::new(reqwest::Client::new(), s.data.path().into()))
        .unwrap()
        .unwrap();
    assert_eq!(channels(&restarted, "provider").await.len(), 2);
    // Once the breaker trips the contribution must still survive subsequent refreshes.
    set(&restarted, "managed-probe", json!({"mode":"fail"})).await;
    for _ in 0..8 {
        assert_eq!(channels(&restarted, "provider").await.len(), 2);
    }
}

#[tokio::test]
async fn snapshots_are_owner_scoped_and_disabled_plugins_stop_contributing() {
    let Some(s) = stage(&["owner-one", "owner-two"]) else { return };
    let first = channels(&s.system, "provider").await;
    assert_eq!(first.len(), 4);
    assert_ne!(first[0].get_uuid(), first[2].get_uuid(), "same guest id must not collide between owners");
    set(&s.system, "owner-one", json!({"mode":"empty"})).await;
    let remaining = channels(&s.system, "provider").await;
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].get_uuid(), first[2].get_uuid());
    let mut config = s.config.clone();
    config.plugins[1].enabled = false;
    s.system.reload(&config);
    assert!(channels(&s.system, "provider").await.is_empty());
    config.plugins[1].enabled = true;
    s.system.reload(&config);
    assert_eq!(channels(&s.system, "provider").await.len(), 2);
    assert!(s.root.path().join("owner-two/plugin.wasm").is_file());
}

#[tokio::test]
async fn purging_one_owner_cannot_remove_another_owners_snapshots() {
    // "managed" is a valid plugin id: the host snapshot root must be outside plugin KV dirs.
    let Some(s) = stage(&["managed", "other-owner"]) else { return };
    assert_eq!(channels(&s.system, "provider").await.len(), 4);
    let snapshots = s.data.path().join(".managed-channels");
    assert!(snapshots.join("managed").is_dir());
    assert!(snapshots.join("other-owner").is_dir());
    set(&s.system, "other-owner", json!({"mode":"fail"})).await;
    let mut config = s.config.clone();
    config.plugins.retain(|entry| entry.id != "managed");
    let guard = s.system.install_lock().await;
    s.system.api_uninstall(&guard, "managed", true, &config).await.unwrap();
    drop(guard);
    assert!(!snapshots.join("managed").exists());
    assert!(snapshots.join("other-owner").is_dir());
    assert_eq!(channels(&s.system, "provider").await.len(), 2, "other owner must retain its last-good fallback");
}

#[tokio::test]
async fn destination_and_stage_snapshots_are_isolated_and_origins_cannot_escape_target() {
    let Some(s) = stage(&["managed-probe"]) else { return };
    let a = target_channels(&s.system, "Alpha", ManagedChannelStage::AfterFilter, &["provider"]).await;
    let b = target_channels(&s.system, "Beta", ManagedChannelStage::AfterFilter, &["provider"]).await;
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert_ne!(a[0].get_uuid(), b[0].get_uuid());
    let before = target_channels(&s.system, "Alpha", ManagedChannelStage::BeforeFilter, &["provider"]).await;
    assert_eq!(a[0].get_uuid(), before[0].get_uuid(), "moving insertion point should preserve channel identity");
    set(&s.system, "managed-probe", json!({"source_input":"outside-target"})).await;
    assert_eq!(
        target_channels(&s.system, "Alpha", ManagedChannelStage::AfterFilter, &["provider"]).await.len(),
        2,
        "invalid response keeps last-good snapshot"
    );
    assert!(target_channels(&s.system, "Unseen", ManagedChannelStage::AfterFilter, &["provider"]).await.is_empty());
    assert!(
        target_channels(&s.system, "Alpha", ManagedChannelStage::AfterFilter, &["other-provider"]).await.is_empty(),
        "revoked input must not survive in fallback"
    );
    set(&s.system, "managed-probe", json!({"mode":"empty"})).await;
    assert!(target_channels(&s.system, "Alpha", ManagedChannelStage::AfterFilter, &["provider"]).await.is_empty());
    set(&s.system, "managed-probe", json!({"mode":"fail"})).await;
    assert_eq!(target_channels(&s.system, "Alpha", ManagedChannelStage::BeforeFilter, &["provider"]).await.len(), 2);
    assert_eq!(target_channels(&s.system, "Beta", ManagedChannelStage::AfterFilter, &["provider"]).await.len(), 2);
}
