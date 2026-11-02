//! Update discovery, eligibility and staging. Catalogs are written straight
//! into the cache file the marketplace reads, so nothing touches the network.
//! Most tests use UI-only packages (no `plugin.wasm`); the MCP and settings-migration ones
//! need a built fixture and skip without it (see `bin/build_plugin_fixture.sh`).

use http::StatusCode;
use serde_json::{json, Value};
use shared::model::{PluginApproveDto, PluginUpdateBlockerDto, PluginUpdatePolicy};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginRepository, PluginsConfig};
use tuliprox_plugins::{
    Blocker, FetchPolicy, InstallGuard, MarketplaceExpect, PluginServices, PluginSystem, UpdateCandidate,
};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

const ID: &str = "demo";
const SOURCE: &str = "main";

struct Env {
    system: PluginSystem,
    config: PluginsConfig,
    data: PathBuf,
    _root: TempDir,
}

fn repository(id: &str, enabled: bool) -> PluginRepository {
    PluginRepository {
        id: id.to_string(),
        name: id.to_string(),
        url: format!("https://{id}.example/marketplace.json"),
        enabled,
    }
}

fn entry(id: &str, enabled: bool, policy: PluginUpdatePolicy, settings_version: u32) -> PluginEntry {
    PluginEntry {
        id: id.to_string(),
        enabled,
        priority: 0,
        settings: serde_json::Map::new(),
        settings_version,
        update_policy: policy,
    }
}

fn env() -> Env {
    let root = tempfile::tempdir().expect("temporary directory");
    let plugins = root.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let data = root.path().join("data");
    let config = PluginsConfig {
        enabled: true,
        directory: plugins.to_string_lossy().to_string(),
        repositories: vec![repository(SOURCE, true)],
        ..PluginsConfig::default()
    };
    let system = PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.clone()))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    Env { system, config, data, _root: root }
}

fn stored() -> SimpleFileOptions { SimpleFileOptions::default().compression_method(CompressionMethod::Stored) }

fn zip_of(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in files {
        writer.start_file(name.as_str(), stored()).unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// A UI-only package `demo`; `extra` is appended to the manifest and `files`
/// are added next to it.
fn package(version: &str, extra: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut all = vec![(
        "manifest.yml".to_string(),
        format!("id: {ID}\nname: Demo\nversion: {version}\napi_version: 1\n{extra}").into_bytes(),
    )];
    all.extend(files.iter().map(|(name, content)| ((*name).to_string(), content.as_bytes().to_vec())));
    zip_of(&all)
}

/// A package with a settings schema at `schema_version`.
fn package_with_schema(version: &str, schema_version: u32) -> Vec<u8> {
    package(
        version,
        "ui:\n  config_section:\n    schema: config-schema.json\n",
        &[("config-schema.json", &json!({"type": "object", "version": schema_version}).to_string())],
    )
}

fn caps_extra(capabilities: &[&str]) -> String {
    if capabilities.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = capabilities.iter().map(|name| format!("  {name}: true")).collect();
    format!("capabilities:\n{}\n", lines.join("\n"))
}

fn approve_body(capabilities: &[&str], mcp: bool) -> PluginApproveDto {
    PluginApproveDto { wasm_sha256: None, capabilities: capabilities.iter().map(ToString::to_string).collect(), mcp }
}

fn expect(id: &str, version: &str, source: &str) -> MarketplaceExpect {
    MarketplaceExpect {
        id: id.to_string(),
        version: version.to_string(),
        origin: shared::model::PluginOriginDto { source: source.to_string(), version: version.to_string() },
    }
}

/// Install `bytes` as a marketplace install from `source`.
async fn install_from(env: &Env, id: &str, version: &str, bytes: Vec<u8>, source: &str, caps: &[&str], mcp: bool) {
    let guard = env.system.install_lock().await;
    env.system.api_upload_from_marketplace(&guard, bytes, expect(id, version, source)).await.expect("stage");
    env.system.api_approve(&guard, id, &approve_body(caps, mcp), &env.config).await.expect("approve");
}

/// Install `demo` 1.0.0 from the main source.
async fn install_demo(env: &Env) {
    install_from(env, ID, "1.0.0", package("1.0.0", "", &[]), SOURCE, &[], false).await;
}

fn release(version: &str, extra: &Value) -> Value {
    let mut release = json!({
        "version": version,
        "package_url": format!("https://plugins.example/{ID}-{version}.tlpx"),
        "min_tuliprox_version": null,
        "plugin_api": 1,
        "capabilities": [],
        "changelog": null,
        "published": null,
    });
    if let (Some(base), Some(extra)) = (release.as_object_mut(), extra.as_object()) {
        base.extend(extra.clone());
    }
    release
}

/// Write the cache of `source` as a sync would have left it.
fn write_catalog(env: &Env, source: &str, id: &str, releases: &[Value]) {
    let url = format!("https://{source}.example/marketplace.json");
    let cache = json!({
        "url": url,
        "catalog": {
            "name": null,
            "plugins": [{
                "id": id, "name": "Demo", "description": null, "author": null, "license": null,
                "category": null, "homepage": null, "releases": releases,
            }],
        },
        "etag": null, "last_modified": null, "last_sync": 1, "last_error": null, "warnings": [],
    });
    let dir = env.data.join("marketplace");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{source}.json")), cache.to_string()).unwrap();
}

async fn candidates(env: &Env) -> Vec<UpdateCandidate> { env.system.update_candidates(&env.config).await }

async fn the_candidate(env: &Env) -> UpdateCandidate {
    let mut found = candidates(env).await;
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

/// Stage the candidate's release as `bytes` through `stage_update`.
async fn stage(env: &Env, candidate: &UpdateCandidate, bytes: Vec<u8>) {
    let guard = env.system.install_lock().await;
    env.system.stage_update(&guard, candidate, bytes).await.expect("staged");
}

// --- candidates -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_newest_compatible_newer_release_is_the_candidate() {
    let env = env();
    install_demo(&env).await;
    write_catalog(
        &env,
        SOURCE,
        ID,
        &[
            release("0.9.0", &json!({})),
            release("1.0.0", &json!({})),
            release("1.1.0", &json!({"capabilities": ["storage", "network", "network"]})),
            release("1.2.0", &json!({"changelog": "More things"})),
            release("1.3.0", &json!({"plugin_api": 2})),
            release("1.4.0", &json!({"min_tuliprox_version": "999.0.0"})),
        ],
    );
    let candidate = the_candidate(&env).await;
    assert_eq!((candidate.id.as_str(), candidate.name.as_str()), (ID, "Demo"));
    assert_eq!((candidate.installed_version.as_str(), candidate.version.as_str()), ("1.0.0", "1.2.0"));
    assert_eq!(candidate.source, SOURCE);
    assert_eq!(candidate.package_url, "https://plugins.example/demo-1.2.0.tlpx");
    assert_eq!(candidate.changelog.as_deref(), Some("More things"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_covered_release_is_offered_beside_a_newest_that_asks_for_more() {
    let env = env();
    install_demo(&env).await;
    write_catalog(
        &env,
        SOURCE,
        ID,
        &[
            release("1.1.0", &json!({})),
            release("1.2.0", &json!({"capabilities": ["network"]})),
            release("1.3.0", &json!({"capabilities": ["network", "storage"]})),
        ],
    );
    let found = candidates(&env).await;
    let versions: Vec<&str> = found.iter().map(|candidate| candidate.version.as_str()).collect();
    assert_eq!(versions, ["1.3.0", "1.1.0"], "newest first, then the newest the approval covers");
    let covered = env.system.evaluate_update(&env.config, &found[1]).await;
    assert!(covered.is_eligible() && !covered.verified);
    let newest = env.system.evaluate_update(&env.config, &found[0]).await;
    assert!(!newest.is_eligible());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_catalog_capability_list_is_sorted_and_deduplicated() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({"capabilities": ["storage", "network", "network"]}))]);
    assert_eq!(the_candidate(&env).await.catalog_capabilities, ["network", "storage"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_equal_or_older_or_incompatible_release_is_no_candidate() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("0.9.0", &json!({})), release("1.0.0", &json!({}))]);
    assert_eq!(candidates(&env).await, Vec::new(), "equal and older");
    write_catalog(
        &env,
        SOURCE,
        ID,
        &[release("2.0.0", &json!({"plugin_api": 2})), release("2.1.0", &json!({"min_tuliprox_version": "999.0.0"}))],
    );
    assert_eq!(candidates(&env).await, Vec::new(), "newer but incompatible");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_without_a_recorded_origin_has_no_candidate() {
    let env = env();
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package("1.0.0", "", &[])).await.unwrap();
    env.system.api_approve(&guard, ID, &approve_body(&[], false), &env.config).await.unwrap();
    drop(guard);
    write_catalog(&env, SOURCE, ID, &[release("2.0.0", &json!({}))]);
    assert_eq!(candidates(&env).await, Vec::new());
    assert_eq!(env.system.update_source_ids(&env.config), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_recorded_source_counts() {
    let mut env = env();
    env.config.repositories.push(repository("other", true));
    install_demo(&env).await;
    // The same id is newer in another source, and nothing newer in the recorded one.
    write_catalog(&env, SOURCE, ID, &[release("1.0.0", &json!({}))]);
    write_catalog(&env, "other", ID, &[release("9.0.0", &json!({}))]);
    assert_eq!(candidates(&env).await, Vec::new());
    // Newer in both: the recorded source's release wins, not the newest overall.
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    assert_eq!((candidate.source.as_str(), candidate.version.as_str()), (SOURCE, "1.1.0"));
    assert_eq!(env.system.update_source_ids(&env.config), [SOURCE]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_or_disabled_or_unsynced_source_gives_no_candidate() {
    let mut env = env();
    install_demo(&env).await;
    assert_eq!(candidates(&env).await, Vec::new(), "never synced");
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    assert_eq!(candidates(&env).await.len(), 1);

    env.config.repositories[0].enabled = false;
    assert_eq!(candidates(&env).await, Vec::new(), "disabled");
    assert_eq!(env.system.update_source_ids(&env.config), Vec::<String>::new());

    env.config.repositories.clear();
    assert_eq!(candidates(&env).await, Vec::new(), "removed");
    assert_eq!(env.system.update_source_ids(&env.config), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn enabled_and_policy_come_from_the_config_entry() {
    let mut env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let bare = the_candidate(&env).await;
    assert_eq!((bare.enabled, bare.policy), (false, PluginUpdatePolicy::Auto));

    env.config.plugins = vec![entry(ID, true, PluginUpdatePolicy::Pinned, 1)];
    let configured = the_candidate(&env).await;
    assert_eq!((configured.enabled, configured.policy), (true, PluginUpdatePolicy::Pinned));
    // the policy never blocks
    let eligibility = env.system.evaluate_update(&env.config, &configured).await;
    assert!(eligibility.is_eligible());
}

// --- eligibility: catalog only ----------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_catalog_judgment_compares_the_listed_capabilities_and_is_not_verified() {
    let env = env();
    install_from(&env, ID, "1.0.0", package("1.0.0", &caps_extra(&["storage"]), &[]), SOURCE, &["storage"], false)
        .await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({"capabilities": ["storage"]}))]);
    let eligibility = env.system.evaluate_update(&env.config, &the_candidate(&env).await).await;
    assert!(!eligibility.verified);
    assert!(eligibility.is_eligible());
    assert_eq!(eligibility.reason(), None);
    assert_eq!(eligibility.capability_diff.unchanged, ["storage"]);

    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({"capabilities": ["storage", "network"]}))]);
    let eligibility = env.system.evaluate_update(&env.config, &the_candidate(&env).await).await;
    assert!(!eligibility.verified);
    assert_eq!(eligibility.blockers, [Blocker::CapabilitiesAdded(vec!["network".to_string()])]);
    assert_eq!(eligibility.capability_diff.added, ["network"]);
    assert!(eligibility.reason().unwrap().contains("network"));
    let dto = PluginUpdateBlockerDto::from(&eligibility.blockers[0]);
    assert_eq!(dto, PluginUpdateBlockerDto::CapabilitiesAdded { capabilities: vec!["network".to_string()] });
}

// --- eligibility: the staged package ----------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn staging_the_release_makes_the_judgment_verified() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    assert!(!env.system.evaluate_update(&env.config, &candidate).await.verified);

    stage(&env, &candidate, package("1.1.0", "", &[])).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert!(eligibility.verified);
    assert!(eligibility.is_eligible());
    // Staging it again is allowed: the slot holds a package of the same source.
    stage(&env, &candidate, package("1.1.0", "", &[])).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_staged_package_decides_when_the_catalog_understated_its_capabilities() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({"capabilities": []}))]);
    let candidate = the_candidate(&env).await;
    assert!(env.system.evaluate_update(&env.config, &candidate).await.is_eligible());

    stage(&env, &candidate, package("1.1.0", &caps_extra(&["network"]), &[])).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert!(eligibility.verified);
    assert_eq!(eligibility.blockers, [Blocker::CapabilitiesAdded(vec!["network".to_string()])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_staged_package_of_another_release_is_not_verified() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let older = the_candidate(&env).await;
    stage(&env, &older, package("1.1.0", "", &[])).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({})), release("1.2.0", &json!({}))]);
    let newer = the_candidate(&env).await;
    assert_eq!(newer.version, "1.2.0");
    assert!(!env.system.evaluate_update(&env.config, &newer).await.verified);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_upload_in_the_slot_is_not_a_verified_release() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package("1.1.0", "", &[])).await.unwrap();
    drop(guard);
    assert!(!env.system.evaluate_update(&env.config, &candidate).await.verified);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settings_schema_that_moves_up_without_a_migration_is_a_blocker() {
    let mut env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    env.config.plugins = vec![entry(ID, false, PluginUpdatePolicy::Auto, 1)];
    let candidate = the_candidate(&env).await;
    stage(&env, &candidate, package_with_schema("1.1.0", 2)).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert!(eligibility.verified);
    assert_eq!(eligibility.blockers, [Blocker::SettingsMigrationMissing { from: 1, to: 2 }]);
    assert!(eligibility.reason().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_settings_schema_below_the_stored_version_is_a_downgrade_blocker() {
    let mut env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    env.config.plugins = vec![entry(ID, false, PluginUpdatePolicy::Auto, 3)];
    let candidate = the_candidate(&env).await;
    stage(&env, &candidate, package_with_schema("1.1.0", 2)).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert_eq!(eligibility.blockers, [Blocker::SettingsVersionDowngrade { from: 3, to: 2 }]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_equal_settings_version_blocks_nothing_and_changes_nothing() {
    let mut env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    env.config.plugins = vec![entry(ID, false, PluginUpdatePolicy::Auto, 2)];
    let candidate = the_candidate(&env).await;
    stage(&env, &candidate, package_with_schema("1.1.0", 2)).await;
    assert!(env.system.evaluate_update(&env.config, &candidate).await.is_eligible());
    assert_eq!(env.config.plugins[0].settings_version, 2);
}

// --- staging ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn stage_update_refuses_a_foreign_staged_upload() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;

    // a plain upload of the same id holds the slot
    let guard: InstallGuard<'_> = env.system.install_lock().await;
    env.system.api_upload(&guard, package("1.0.5", "", &[("mine.txt", "x")])).await.unwrap();
    let rejection = env.system.stage_update(&guard, &candidate, package("1.1.0", "", &[])).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);
    assert!(env.data.join("staging").join(ID).join("mine.txt").is_file(), "the foreign upload is untouched");
    drop(guard);

    // so does a marketplace package of another source
    let guard = env.system.install_lock().await;
    fs::remove_dir_all(env.data.join("staging").join(ID)).unwrap();
    env.system
        .api_upload_from_marketplace(&guard, package("1.0.5", "", &[]), expect(ID, "1.0.5", "other"))
        .await
        .unwrap();
    let rejection = env.system.stage_update(&guard, &candidate, package("1.1.0", "", &[])).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_update_refuses_a_package_that_is_not_the_release() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    let guard = env.system.install_lock().await;
    let rejection = env.system.stage_update(&guard, &candidate, package("1.9.0", "", &[])).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!env.data.join("staging").join(ID).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_staged_update_is_approved_through_the_existing_route() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    stage(&env, &candidate, package("1.1.0", "", &[])).await;
    let guard = env.system.install_lock().await;
    let summary = env.system.api_approve(&guard, ID, &approve_body(&[], false), &env.config).await.unwrap();
    assert_eq!(summary.version.as_deref(), Some("1.1.0"));
    assert_eq!(summary.origin.map(|origin| origin.version), Some("1.1.0".to_string()));
    drop(guard);
    assert_eq!(candidates(&env).await, Vec::new(), "now up to date");
}

#[tokio::test(flavor = "multi_thread")]
async fn download_update_refuses_a_plain_http_package_url() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({"package_url": "http://127.0.0.1:9/demo.tlpx"}))]);
    let candidate = the_candidate(&env).await;
    let policy = FetchPolicy { allow_http: false };
    let error = env.system.download_update(&reqwest::Client::new(), policy, &candidate).await.unwrap_err();
    assert!(!error.to_string().contains("127.0.0.1"), "the error never carries the URL");
}

// --- the DTO ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_updates_dto_carries_each_verdict_and_the_source_errors() {
    let env = env();
    install_demo(&env).await;
    write_catalog(&env, SOURCE, ID, &[release("1.1.0", &json!({"capabilities": ["network"], "changelog": "c"}))]);
    let errors =
        vec![shared::model::PluginUpdateSourceErrorDto { source: "gone".to_string(), error: "boom".to_string() }];
    let dto = env.system.updates_dto(&env.config, errors.clone()).await;
    assert_eq!(dto.errors, errors);
    assert_eq!(dto.candidates.len(), 1);
    let row = &dto.candidates[0];
    assert_eq!((row.id.as_str(), row.version.as_str(), row.source.as_str()), (ID, "1.1.0", SOURCE));
    assert!(!row.eligible && !row.verified);
    assert_eq!(row.blockers, [PluginUpdateBlockerDto::CapabilitiesAdded { capabilities: vec!["network".to_string()] }]);
    assert_eq!(row.changelog.as_deref(), Some("c"));
    assert_eq!(row.capability_diff.added, ["network"]);
}

// --- fixture-backed: MCP exposure and the migration dry run -------------------

fn fixture_root(name: &str) -> Option<PathBuf> {
    let directory = std::env::var_os("TULIPROX_PLUGIN_FIXTURES").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-fixtures/plugins"),
        Into::into,
    );
    let root = directory.join(name);
    if root.join("plugin.wasm").is_file() {
        Some(root)
    } else {
        eprintln!("skipped: fixture `{name}` not built; run bin/build_plugin_fixture.sh");
        None
    }
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            files.push((name, fs::read(&path).unwrap()));
        }
    }
}

/// The fixture as a `.tlpx`, its manifest passed through `edit`.
fn fixture_package(root: &Path, edit: impl Fn(&str) -> String) -> Vec<u8> {
    let mut files = Vec::new();
    collect(root, root, &mut files);
    for (name, content) in &mut files {
        if name == "manifest.yml" {
            *content = edit(&String::from_utf8(content.clone()).unwrap()).into_bytes();
        }
    }
    zip_of(&files)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_release_that_exposes_more_mcp_tools_than_the_installed_one_is_blocked() {
    let Some(root) = fixture_root("ui-demo") else { return };
    let env = env();
    // installed 1.0.0 offers `echo` and `counter` as tools, not `bump`
    let installed = fixture_package(&root, |text| {
        text.replace("version: 0.1.0", "version: 1.0.0")
            .replace("      mcp: true\n      description: Adds", "      mcp: false\n      description: Adds")
    });
    install_from(&env, "ui-demo", "1.0.0", installed, SOURCE, &["storage"], true).await;
    write_catalog(&env, SOURCE, "ui-demo", &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    let update = fixture_package(&root, |text| text.replace("version: 0.1.0", "version: 1.1.0"));
    stage(&env, &candidate, update).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert!(eligibility.verified);
    assert_eq!(eligibility.blockers, [Blocker::McpExposureWidened(vec!["bump".to_string()])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_mcp_approval_a_wider_exposure_is_not_a_blocker() {
    let Some(root) = fixture_root("ui-demo") else { return };
    let env = env();
    let installed = fixture_package(&root, |text| {
        text.replace("version: 0.1.0", "version: 1.0.0")
            .replace("      mcp: true\n      description: Adds", "      mcp: false\n      description: Adds")
    });
    install_from(&env, "ui-demo", "1.0.0", installed, SOURCE, &["storage"], false).await;
    write_catalog(&env, SOURCE, "ui-demo", &[release("1.1.0", &json!({}))]);
    let candidate = the_candidate(&env).await;
    stage(&env, &candidate, fixture_package(&root, |text| text.replace("version: 0.1.0", "version: 1.1.0"))).await;
    assert!(env.system.evaluate_update(&env.config, &candidate).await.is_eligible());
}

const MIGRATE_ID: &str = "settings-migrate-probe";

/// The probe's schema is at version 2; install a version 1 look-alike first.
async fn migrate_env(root: &Path, stored_settings: Value) -> (Env, UpdateCandidate) {
    let mut env = env();
    let installed = zip_of(&[
        (
            "manifest.yml".to_string(),
            format!("id: {MIGRATE_ID}\nname: Probe\nversion: 0.1.0\napi_version: 1\nui:\n  config_section:\n    schema: config-schema.json\n")
                .into_bytes(),
        ),
        ("config-schema.json".to_string(), json!({"type": "object"}).to_string().into_bytes()),
    ]);
    install_from(&env, MIGRATE_ID, "0.1.0", installed, SOURCE, &[], false).await;
    write_catalog(&env, SOURCE, MIGRATE_ID, &[release("0.2.0", &json!({}))]);
    let mut stored = entry(MIGRATE_ID, false, PluginUpdatePolicy::Auto, 1);
    stored.settings = stored_settings.as_object().cloned().unwrap_or_default();
    env.config.plugins = vec![stored];
    let candidate = the_candidate(&env).await;
    stage(&env, &candidate, fixture_package(root, str::to_string)).await;
    (env, candidate)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dry_run_accepts_settings_the_release_migrates_and_persists_nothing() {
    let Some(root) = fixture_root(MIGRATE_ID) else { return };
    let (env, candidate) = migrate_env(&root, json!({"label": "News"})).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert!(eligibility.verified);
    assert!(eligibility.is_eligible(), "{:?}", eligibility.blockers);
    // nothing was written, and the settings stay at their stored version
    assert_eq!(env.config.plugins[0].settings_version, 1);
    assert_eq!(env.config.plugins[0].settings.get("label"), Some(&json!("News")));
    // and a failed dry run is not remembered either: the same call gives the same answer
    assert!(env.system.evaluate_update(&env.config, &candidate).await.is_eligible());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_migration_is_a_blocker_with_the_reason() {
    let Some(root) = fixture_root(MIGRATE_ID) else { return };
    let (env, candidate) = migrate_env(&root, json!({"label": "fail"})).await;
    let eligibility = env.system.evaluate_update(&env.config, &candidate).await;
    assert!(eligibility.verified);
    match eligibility.blockers.as_slice() {
        [Blocker::SettingsMigrationFailed { from: 1, to: 2, reason }] => assert_ne!(reason, ""),
        other => panic!("unexpected blockers: {other:?}"),
    }
    assert!(eligibility.reason().unwrap().contains("failed"));
    // the installed plugin shows no settings review for the attempt
    let summary = env.system.api_summary(MIGRATE_ID, Some(&env.config)).expect("installed");
    assert!(summary.settings_review.is_none(), "{:?}", summary.settings_review);
}
