//! Update and rollback over
//! UI-only packages, which need no built fixture. The activation check is
//! driven by hand: a UI-only package has no `init` to fail.

use super::*;
use crate::PluginServices;
use serde_json::json;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

struct Env {
    system: PluginSystem,
    config: PluginsConfig,
    plugins: PathBuf,
    data: PathBuf,
    _root: tempfile::TempDir,
}

fn env_with(entry_settings: Option<Value>) -> Env {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let data = root.path().join("data");
    let mut config =
        PluginsConfig { enabled: true, directory: plugins.to_string_lossy().to_string(), ..PluginsConfig::default() };
    if let Some(Value::Object(settings)) = entry_settings {
        config.plugins.push(PluginEntry {
            id: "demo".to_string(),
            enabled: false,
            priority: 0,
            settings,
            settings_version: 1,
            update_policy: shared::model::PluginUpdatePolicy::Auto,
        });
    }
    let system = PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.clone()))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    Env { system, config, plugins, data, _root: root }
}

fn env() -> Env { env_with(None) }

fn package(version: &str, capabilities: &[&str]) -> Vec<u8> {
    let mut text = format!("id: demo\nname: Demo\nversion: {version}\napi_version: 1\n");
    if !capabilities.is_empty() {
        text.push_str("capabilities:\n");
        text.push_str(&capabilities.iter().map(|name| format!("  {name}: true")).collect::<Vec<_>>().join("\n"));
        text.push('\n');
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

async fn upload_and_approve(env: &Env, version: &str, capabilities: &[&str]) {
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package(version, capabilities)).await.expect("upload");
    env.system.api_approve(&guard, "demo", &body(capabilities), &env.config).await.expect("approve");
}

fn pin_capabilities(env: &Env) -> Vec<String> {
    read_approvals(&approvals_path(&env.data)).get("demo").map(|pin| pin.capabilities.clone()).unwrap_or_default()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(dir) {
        Ok(entries) => entries.map(|entry| entry.unwrap().file_name().to_string_lossy().to_string()).collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn no_strings() -> Vec<String> { Vec::new() }

fn installed_version(env: &Env) -> Option<String> {
    env.system.api_summary("demo", None).and_then(|summary| summary.version)
}

#[test]
fn the_capability_diff_marks_added_removed_and_unchanged_and_never_carries_mcp() {
    let pin = ApprovalPin {
        wasm_sha256: String::new(),
        capabilities: vec!["network".to_string(), "storage".to_string()],
        mcp: true,
        origin: None,
    };
    let wanted = vec!["epg_write".to_string(), "storage".to_string()];
    let diff = capability_diff(&wanted, Some(&pin));
    assert_eq!(diff.added, ["epg_write"]);
    assert_eq!(diff.removed, ["network"]);
    assert_eq!(diff.unchanged, ["storage"]);
    assert!(diff.mcp_was_approved);
    let none = capability_diff(&wanted, None);
    assert_eq!(none.added, wanted);
    assert!(none.removed.is_empty() && none.unchanged.is_empty() && !none.mcp_was_approved);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_update_with_a_new_capability_needs_it_in_the_approve_body() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &["storage"]).await;

    let guard = env.system.install_lock().await;
    let review = env.system.api_upload(&guard, package("1.1.0", &["storage", "network"])).await.unwrap();
    assert_eq!(review.replaces.as_deref(), Some("1.0.0"));
    let diff = review.capability_diff.expect("an update carries the diff");
    assert_eq!((diff.added, diff.removed, diff.unchanged), (vec!["network".into()], vec![], vec!["storage".into()]));
    let update = env.system.update_summary("demo").expect("the staged update shows in the summary");
    assert_eq!(update.version, "1.1.0");
    assert_eq!(update.added_capabilities, ["network"]);

    // Echoing only what was approved before is not enough.
    let rejection = env.system.api_approve(&guard, "demo", &body(&["storage"]), &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);
    assert_eq!(installed_version(&env).as_deref(), Some("1.0.0"));
    assert_eq!(pin_capabilities(&env), ["storage"]);
    assert!(env.data.join("staging/demo").is_dir(), "the review can be retried");
    assert!(env.system.rollback_summary("demo").is_none(), "nothing was swapped");

    env.system.api_approve(&guard, "demo", &body(&["network", "storage"]), &env.config).await.unwrap();
    assert_eq!(installed_version(&env).as_deref(), Some("1.1.0"));
    assert_eq!(pin_capabilities(&env), ["network", "storage"]);
    assert!(env.system.update_summary("demo").is_none(), "the staged copy was consumed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_staged_update_can_be_reviewed_again_without_uploading_it() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &[]).await;
    let guard = env.system.install_lock().await;
    let rejection = env.system.api_staged_review(&guard, "demo").await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT, "nothing staged, and the installed one is approved");
    let uploaded = env.system.api_upload(&guard, package("1.1.0", &["network"])).await.unwrap();
    let again = env.system.api_staged_review(&guard, "demo").await.unwrap();
    assert_eq!(again, uploaded);
    assert_eq!(again.capability_diff.unwrap().added, ["network"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_shipped_in_the_plugins_directory_is_reviewed_and_approved_in_place() {
    let root = tempfile::tempdir().unwrap();
    let plugins = root.path().join("plugins");
    let shipped = plugins.join("demo");
    fs::create_dir_all(&shipped).unwrap();
    fs::write(
        shipped.join("manifest.yml"),
        "id: demo\nname: Demo\nversion: 1.0.0\napi_version: 1\ncapabilities:\n  storage: true\n",
    )
    .unwrap();
    let config =
        PluginsConfig { enabled: true, directory: plugins.to_string_lossy().to_string(), ..PluginsConfig::default() };
    let system = PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), root.path().join("data")))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    let guard = system.install_lock().await;

    let review = system.api_staged_review(&guard, "demo").await.expect("the shipped plugin can be reviewed");
    assert_eq!(review.version, "1.0.0");
    assert_eq!(review.capabilities.iter().map(|capability| capability.name.as_str()).collect::<Vec<_>>(), ["storage"]);
    assert!(review.replaces.is_none() && review.capability_diff.is_none(), "it replaces nothing");

    system.api_approve(&guard, "demo", &body(&["storage"]), &config).await.expect("approve");
    assert!(shipped.join("manifest.yml").is_file(), "approved in place");
    let rejection = system.api_staged_review(&guard, "demo").await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT, "nothing is waiting for approval any more");
    assert_eq!(system.api_staged_review(&guard, "missing").await.unwrap_err().status, StatusCode::NOT_FOUND);

    // A changed capability list puts it back to `pending_approval`; the review
    // shows what changed against the earlier approval.
    fs::write(
        shipped.join("manifest.yml"),
        "id: demo\nname: Demo\nversion: 1.0.1\napi_version: 1\ncapabilities:\n  network: true\n",
    )
    .unwrap();
    system.reload(&config);
    let again = system.api_staged_review(&guard, "demo").await.expect("the changed plugin can be reviewed");
    assert!(again.replaces.is_none(), "still approved in place");
    let diff = again.capability_diff.expect("the earlier approval is compared");
    assert_eq!((diff.added, diff.removed), (vec!["network".to_string()], vec!["storage".to_string()]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_install_has_no_diff_and_no_prior() {
    let env = env();
    let guard = env.system.install_lock().await;
    let review = env.system.api_upload(&guard, package("1.0.0", &[])).await.unwrap();
    assert!(review.replaces.is_none() && review.capability_diff.is_none());
    env.system.api_approve(&guard, "demo", &body(&[]), &env.config).await.unwrap();
    assert!(env.system.rollback_summary("demo").is_none());
    assert!(!env.data.join(PRIOR_DIR).join("demo").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_one_prior_version_is_retained() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &[]).await;
    upload_and_approve(&env, "2.0.0", &["storage"]).await;
    assert_eq!(env.system.rollback_summary("demo").unwrap().version, "1.0.0");
    upload_and_approve(&env, "3.0.0", &[]).await;

    assert_eq!(installed_version(&env).as_deref(), Some("3.0.0"));
    assert_eq!(env.system.rollback_summary("demo").unwrap().version, "2.0.0", "the older prior was replaced");
    let prior = env.data.join(PRIOR_DIR).join("demo");
    assert_eq!(names(&prior), [PRIOR_PACKAGE, PRIOR_META], "sorted: the package and its prev.json, nothing else");
    assert!(prior.join(PRIOR_PACKAGE).join("v2.0.0.txt").is_file());
    assert!(!prior.join(PRIOR_PACKAGE).join("v1.0.0.txt").exists());
    let meta = read_prior_meta(&prior).unwrap();
    assert_eq!(meta.pin.unwrap().capabilities, ["storage"]);
    assert_eq!(names(&env.data.join(PRIOR_DIR)), ["demo"], "no swap leftovers");
    assert_eq!(names(&env.plugins), ["demo"], "no swap leftovers");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_rollback_swaps_the_packages_the_pins_and_the_prior() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &[]).await;
    upload_and_approve(&env, "2.0.0", &["storage"]).await;
    assert_eq!(pin_capabilities(&env), ["storage"]);

    let guard = env.system.install_lock().await;
    let outcome = env.system.api_rollback(&guard, "demo", &env.config).await.unwrap();
    assert_eq!(outcome.version, "1.0.0");
    assert!(outcome.settings.is_none(), "no config entry, nothing to restore");
    assert_eq!(installed_version(&env).as_deref(), Some("1.0.0"));
    assert_eq!(pin_capabilities(&env), no_strings());
    assert!(env.plugins.join("demo/v1.0.0.txt").is_file() && !env.plugins.join("demo/v2.0.0.txt").exists());
    assert_eq!(env.system.rollback_summary("demo").unwrap().version, "2.0.0");
    assert_eq!(names(&env.plugins), ["demo"]);
    assert_eq!(names(&env.data.join(PRIOR_DIR)), ["demo"]);

    // and forward again
    let outcome = env.system.api_rollback(&guard, "demo", &env.config).await.unwrap();
    assert_eq!(outcome.version, "2.0.0");
    assert_eq!(pin_capabilities(&env), ["storage"]);
    assert_eq!(env.system.rollback_summary("demo").unwrap().version, "1.0.0");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rollback_without_a_prior_is_a_404_and_an_edited_prior_is_refused() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &[]).await;
    let guard = env.system.install_lock().await;
    let rejection = env.system.api_rollback(&guard, "demo", &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::NOT_FOUND);
    let rejection = env.system.api_rollback(&guard, "nope", &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::NOT_FOUND);

    upload_and_approve_locked(&env, &guard, "2.0.0").await;
    // a prior approved with a wasm hash it no longer has
    let prior = env.data.join(PRIOR_DIR).join("demo");
    let mut meta = read_prior_meta(&prior).unwrap();
    meta.pin = Some(ApprovalPin { wasm_sha256: "ab".repeat(32), capabilities: Vec::new(), mcp: false, origin: None });
    fs::write(prior.join(PRIOR_META), serde_json::to_vec(&meta).unwrap()).unwrap();
    let rejection = env.system.api_rollback(&guard, "demo", &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);
    assert_eq!(installed_version(&env).as_deref(), Some("2.0.0"));
}

async fn upload_and_approve_locked(env: &Env, guard: &InstallGuard<'_>, version: &str) {
    env.system.api_upload(guard, package(version, &[])).await.expect("upload");
    env.system.api_approve(guard, "demo", &body(&[]), &env.config).await.expect("approve");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rollback_restores_the_settings_the_prior_ran_with() {
    let env = env_with(Some(json!({"mode": "old"})));
    upload_and_approve(&env, "1.0.0", &[]).await;
    upload_and_approve(&env, "2.0.0", &[]).await;
    // the operator saved other settings while 2.0.0 ran
    let mut now = env.config.clone();
    now.plugins[0].settings = json!({"mode": "new"}).as_object().unwrap().clone();

    let guard = env.system.install_lock().await;
    let outcome = env.system.api_rollback(&guard, "demo", &now).await.unwrap();
    assert_eq!(outcome.settings, json!({"mode": "old"}).as_object().cloned());
    // the entry the prior now holds is the one that was current
    let meta = read_prior_meta(&env.data.join(PRIOR_DIR).join("demo")).unwrap();
    assert_eq!(meta.entry.unwrap().settings, *json!({"mode": "new"}).as_object().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_activation_after_an_update_restores_the_old_package_and_pin() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &["storage"]).await;
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package("2.0.0", &["network"])).await.unwrap();

    let replacement = env.system.approve_blocking("demo", &body(&["network"]), &env.config).unwrap().unwrap();
    assert_eq!(installed_version(&env).as_deref(), Some("2.0.0"), "swapped, not yet confirmed");
    assert_eq!(pin_capabilities(&env), ["network"]);

    let rejection =
        env.system.finish_replacement(replacement, &env.config, Err("init exploded".to_string())).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);
    assert_eq!(rejection.body.error, "rolled back to 1.0.0: init exploded");
    assert_eq!(installed_version(&env).as_deref(), Some("1.0.0"));
    assert_eq!(pin_capabilities(&env), ["storage"]);
    assert!(env.plugins.join("demo/v1.0.0.txt").is_file());
    assert!(!env.data.join("staging/demo").exists(), "the failed package is discarded");
    assert!(env.system.rollback_summary("demo").is_none(), "the prior is as it was before the update");
    assert_eq!(names(&env.plugins), ["demo"]);
    assert_eq!(names(&env.data.join(PRIOR_DIR)), no_strings());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_update_keeps_the_earlier_prior_and_a_failed_rollback_changes_nothing() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &[]).await;
    upload_and_approve(&env, "2.0.0", &[]).await;
    let guard = env.system.install_lock().await;

    env.system.api_upload(&guard, package("3.0.0", &[])).await.unwrap();
    let replacement = env.system.approve_blocking("demo", &body(&[]), &env.config).unwrap().unwrap();
    env.system.finish_replacement(replacement, &env.config, Err("no".to_string())).await.unwrap_err();
    assert_eq!(installed_version(&env).as_deref(), Some("2.0.0"));
    assert_eq!(env.system.rollback_summary("demo").unwrap().version, "1.0.0", "the old prior survived");
    assert!(env.data.join(PRIOR_DIR).join("demo").join(PRIOR_PACKAGE).join("v1.0.0.txt").is_file());

    let (replacement, _candidate, _outcome) = env.system.rollback_blocking("demo", &env.config).unwrap();
    assert_eq!(installed_version(&env).as_deref(), Some("1.0.0"));
    let rejection =
        env.system.finish_replacement(replacement, &env.config, Err("init failed".to_string())).await.unwrap_err();
    assert_eq!(rejection.body.error, "rollback to 1.0.0 failed and was undone, still on 2.0.0: init failed");
    assert_eq!(installed_version(&env).as_deref(), Some("2.0.0"));
    assert_eq!(env.system.rollback_summary("demo").unwrap().version, "1.0.0");
    assert_eq!(names(&env.plugins), ["demo"]);
    assert_eq!(names(&env.data.join(PRIOR_DIR)), ["demo"]);
}

#[test]
fn a_swap_killed_halfway_leaves_hidden_leftovers_the_next_swap_sweeps() {
    let root = tempfile::tempdir().unwrap();
    let (plugins, prior_root) = (root.path().join("plugins"), root.path().join("plugins-prev"));
    fs::create_dir_all(plugins.join("demo")).unwrap();
    fs::write(plugins.join("demo/manifest.yml"), "m").unwrap();
    fs::create_dir_all(plugins.join(".swap~old~demo~1~1")).unwrap();
    fs::create_dir_all(plugins.join(".swap~old~demo-two~1~1")).unwrap();
    fs::create_dir_all(prior_root.join(".swap~bak~demo~1~2")).unwrap();
    let meta = PriorMeta { version: "1.0.0".to_string(), pin: None, entry: None };
    let paths = SwapPaths::for_swap(&plugins, &prior_root, "demo");
    let swap = FsSwap::prepare(Incoming::Prior, paths, false, &meta);
    // a first swap has no prior package to copy in, so it fails, but only after sweeping
    assert!(swap.is_err());
    assert_eq!(names(&plugins), [".swap~old~demo-two~1~1", "demo"], "another plugin's names are left alone");
    assert_eq!(names(&prior_root), no_strings());
}

// --- marketplace provenance ---------

fn expect(version: &str) -> MarketplaceExpect {
    MarketplaceExpect {
        id: "demo".to_string(),
        version: version.to_string(),
        origin: PluginOriginDto { source: "main".to_string(), version: version.to_string() },
    }
}

fn pin_origin(env: &Env) -> Option<PluginOriginDto> {
    read_approvals(&approvals_path(&env.data)).get("demo").and_then(|pin| pin.origin.clone())
}

fn origin_file(env: &Env) -> PathBuf { env.data.join("staging/.origin/demo.json") }

#[tokio::test(flavor = "multi_thread")]
async fn a_marketplace_install_records_the_origin_on_approve() {
    let env = env();
    let guard = env.system.install_lock().await;
    env.system.api_upload_from_marketplace(&guard, package("1.0.0", &[]), expect("1.0.0")).await.unwrap();
    assert!(origin_file(&env).is_file(), "the origin is pending while staged");
    assert!(pin_origin(&env).is_none());

    let summary = env.system.api_approve(&guard, "demo", &body(&[]), &env.config).await.unwrap();
    let origin = pin_origin(&env).expect("the pin carries the origin");
    assert_eq!((origin.source.as_str(), origin.version.as_str()), ("main", "1.0.0"));
    assert_eq!(summary.origin.as_ref(), Some(&origin));
    assert!(!origin_file(&env).exists(), "the pending origin moved into the pin");
    assert_eq!(env.system.marketplace_installed()["demo"].origin.as_ref(), Some(&origin));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_upload_clears_the_pending_origin_and_installs_without_one() {
    let env = env();
    let guard = env.system.install_lock().await;
    env.system.api_upload_from_marketplace(&guard, package("1.0.0", &[]), expect("1.0.0")).await.unwrap();
    env.system.api_upload(&guard, package("1.0.0", &[])).await.unwrap();
    assert!(!origin_file(&env).exists());
    env.system.api_approve(&guard, "demo", &body(&[]), &env.config).await.unwrap();
    assert!(pin_origin(&env).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_origin_travels_with_the_pin_through_an_update_and_a_rollback() {
    let env = env();
    let guard = env.system.install_lock().await;
    env.system.api_upload_from_marketplace(&guard, package("1.0.0", &[]), expect("1.0.0")).await.unwrap();
    env.system.api_approve(&guard, "demo", &body(&[]), &env.config).await.unwrap();
    // A plain-upload update has no origin.
    env.system.api_upload(&guard, package("1.1.0", &[])).await.unwrap();
    env.system.api_approve(&guard, "demo", &body(&[]), &env.config).await.unwrap();
    assert!(pin_origin(&env).is_none());
    // Rolling back restores the prior pin together with its origin.
    env.system.api_rollback(&guard, "demo", &env.config).await.unwrap();
    assert_eq!(pin_origin(&env).map(|origin| origin.version), Some("1.0.0".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_that_is_not_the_requested_release_is_refused_before_staging_changes() {
    let env = env();
    upload_and_approve(&env, "1.0.0", &[]).await;
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package("1.1.0", &[])).await.unwrap();
    let staged_before = names(&env.data.join("staging"));

    // Wrong version.
    let rejection =
        env.system.api_upload_from_marketplace(&guard, package("1.2.0", &[]), expect("1.1.5")).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Wrong id.
    let mut other = expect("1.2.0");
    other.id = "other".to_string();
    let rejection = env.system.api_upload_from_marketplace(&guard, package("1.2.0", &[]), other).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::UNPROCESSABLE_ENTITY);

    assert_eq!(names(&env.data.join("staging")), staged_before, "no extraction leftovers");
    assert!(env.data.join("staging/demo/v1.1.0.txt").is_file(), "the staged update is untouched");
    assert_eq!(installed_version(&env).as_deref(), Some("1.0.0"));
    assert!(!origin_file(&env).exists());
}
