//! Upload, review, approve, enable, uninstall and the `ext.<section>` settings
//! check, end to end over UI-only packages (no `plugin.wasm`), so none of it
//! needs a built fixture.

use http::StatusCode;
use serde_json::{json, Value};
use shared::model::{PluginApproveDto, PluginStateDto, REDACTED_SECRET};
use std::{
    fs,
    io::{Cursor, Write},
    path::PathBuf,
};
use tempfile::TempDir;
use tuliprox_core::model::{PluginEntry, PluginsConfig};
use tuliprox_plugins::{PluginServices, PluginSystem};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

struct Env {
    system: PluginSystem,
    config: PluginsConfig,
    plugins: PathBuf,
    data: PathBuf,
    _root: TempDir,
}

fn env() -> Env {
    let root = tempfile::tempdir().expect("temporary directory");
    let plugins = root.path().join("plugins");
    fs::create_dir_all(&plugins).unwrap();
    let data = root.path().join("data");
    let config =
        PluginsConfig { enabled: true, directory: plugins.to_string_lossy().to_string(), ..PluginsConfig::default() };
    let system = PluginSystem::load(&config, PluginServices::new(reqwest::Client::new(), data.clone()))
        .expect("plugin system loads")
        .expect("plugins are enabled");
    Env { system, config, plugins, data, _root: root }
}

fn manifest(id: &str, version: &str, extra: &str) -> String {
    format!("id: {id}\nname: Demo\nversion: {version}\napi_version: 1\n{extra}")
}

fn stored() -> SimpleFileOptions { SimpleFileOptions::default().compression_method(CompressionMethod::Stored) }

/// A `.tlpx` with a `manifest.yml` and `files`.
fn package(manifest_text: &str, files: &[(&str, &str)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("manifest.yml", stored()).unwrap();
    writer.write_all(manifest_text.as_bytes()).unwrap();
    for (name, content) in files {
        writer.start_file(*name, stored()).unwrap();
        writer.write_all(content.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

fn approve_body(capabilities: &[&str]) -> PluginApproveDto {
    PluginApproveDto {
        wasm_sha256: None,
        capabilities: capabilities.iter().map(ToString::to_string).collect(),
        mcp: false,
    }
}

/// Upload and approve `bytes` as `id`.
async fn install(env: &Env, id: &str, bytes: Vec<u8>, capabilities: &[&str]) {
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, bytes).await.expect("upload");
    env.system.api_approve(&guard, id, &approve_body(capabilities), &env.config).await.expect("approve");
}

fn staging_entries(env: &Env) -> Vec<String> {
    match fs::read_dir(env.data.join("staging")) {
        Ok(entries) => entries.map(|entry| entry.unwrap().file_name().to_string_lossy().to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

fn entry_with(id: &str, settings: Value) -> PluginEntry {
    let Value::Object(settings) = settings else { panic!("settings must be an object") };
    PluginEntry { id: id.to_string(), enabled: false, priority: 0, settings, settings_version: 1 }
}

const EXTENSION: &str = "ui:\n  config_extensions:\n    - section: proxy\n      schema:\n        type: object\n        properties:\n          port: {type: integer}\n          key: {type: string, writeOnly: true}\n";

// --- upload and review ------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn upload_stages_the_package_and_reviews_every_capability() {
    let env = env();
    let guard = env.system.install_lock().await;
    let caps = "capabilities:\n  network: true\n  storage: true\n  epg_write: true\n  image_cache: true\n  notifications: true\n  stream_headers: true\n";
    let review = env.system.api_upload(&guard, package(&manifest("demo", "1.0.0", caps), &[])).await.unwrap();

    assert_eq!(review.id, "demo");
    assert_eq!(review.version, "1.0.0");
    assert_eq!(review.wasm_sha256, None);
    let names: Vec<&str> = review.capabilities.iter().map(|capability| capability.name.as_str()).collect();
    assert_eq!(names, ["network", "storage", "epg_write", "image_cache", "notifications", "stream_headers"]);
    assert!(review.capabilities.iter().all(|capability| !capability.description.is_empty()));
    assert_eq!(
        review.capabilities[0].description,
        "Unrestricted network access: any public, private or loopback HTTP(S) destination."
    );
    assert!(!review.custom_ui && !review.mcp_requested && review.replaces.is_none());
    // staged, not installed
    assert!(env.data.join("staging/demo/manifest.yml").is_file());
    assert!(!env.plugins.join("demo").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_review_lists_secrets_custom_ui_and_sanitizes_descriptions() {
    let env = env();
    let guard = env.system.install_lock().await;
    let extra = format!(
        "description: \"bell\\u0007 {}\"\nui:\n  config_section:\n    schema: config-schema.json\n  contributions:\n    - slot: menu\n      title: Menu\n      element: ui/menu.html\n",
        "x".repeat(600)
    );
    let schema =
        r#"{"type":"object","properties":{"token":{"type":"string","writeOnly":true},"name":{"type":"string"}}}"#;
    let review = env
        .system
        .api_upload(&guard, package(&manifest("demo", "1.0.0", &extra), &[("config-schema.json", schema)]))
        .await
        .unwrap();

    assert_eq!(review.secrets, ["token"]);
    assert!(review.custom_ui);
    assert_eq!(review.contributions.len(), 1);
    let description = review.description.unwrap();
    assert!(description.starts_with("bell "), "{description}");
    assert!(description.chars().all(|c| !c.is_control()));
    assert_eq!(description.chars().count(), 512);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_package_with_validation_problems_is_a_422_and_leaves_no_staging() {
    let env = env();
    let guard = env.system.install_lock().await;
    // an unknown extension section, and a locale file that is not declared properly
    let extra = "ui:\n  locales: [en]\n  config_extensions:\n    - section: nonsense\n      schema: {type: object}\n";
    let rejection = env.system.api_upload(&guard, package(&manifest("demo", "1.0.0", extra), &[])).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(rejection.body.problems.iter().any(|problem| problem.contains("nonsense")));
    assert!(rejection.body.problems.iter().any(|problem| problem.contains("en.json")));
    assert_eq!(staging_entries(&env), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wasm_that_is_not_a_component_is_a_422() {
    let env = env();
    let guard = env.system.install_lock().await;
    let rejection = env
        .system
        .api_upload(
            &guard,
            package(&manifest("demo", "1.0.0", "hooks: [playlist.after-filter]\n"), &[("plugin.wasm", "not wasm")]),
        )
        .await
        .unwrap_err();
    assert_eq!(rejection.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(rejection.body.problems.iter().any(|problem| problem.contains("plugin.wasm")));
    assert_eq!(staging_entries(&env), Vec::<String>::new());
}

// --- extraction through the route's entry point ----------------------------

async fn upload_refused(env: &Env, bytes: Vec<u8>, needle: &str) {
    let guard = env.system.install_lock().await;
    let rejection = env.system.api_upload(&guard, bytes).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::UNPROCESSABLE_ENTITY);
    let problems = rejection.body.problems.join("; ");
    assert!(problems.contains(needle), "`{problems}` does not mention `{needle}`");
    assert_eq!(staging_entries(env), Vec::<String>::new(), "the staging directory is cleaned");
    assert!(!env.data.join("evil.txt").exists() && !env.plugins.join("evil.txt").exists());
}

fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in entries {
        writer.start_file(*name, stored()).unwrap();
        writer.write_all(content).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_archives_are_refused_and_cleaned_up() {
    let env = env();
    let good = manifest("demo", "1.0.0", "");
    upload_refused(&env, zip_with(&[("manifest.yml", good.as_bytes()), ("../evil.txt", b"x")]), "..").await;
    upload_refused(&env, zip_with(&[("manifest.yml", good.as_bytes()), ("/evil.txt", b"x")]), "absolute").await;

    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer.start_file("manifest.yml", stored()).unwrap();
    writer.write_all(good.as_bytes()).unwrap();
    writer.add_symlink("evil.txt", "/etc/passwd", stored()).unwrap();
    upload_refused(&env, writer.finish().unwrap().into_inner(), "symlink").await;

    let mut many = ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..600 {
        many.start_file(format!("f{index}"), stored()).unwrap();
    }
    upload_refused(&env, many.finish().unwrap().into_inner(), "entries").await;

    // an inflated-size bomb whose headers claim one byte
    let mut bomb = ZipWriter::new(Cursor::new(Vec::new()));
    bomb.start_file("manifest.yml", SimpleFileOptions::default().compression_method(CompressionMethod::Deflated))
        .unwrap();
    let chunk = vec![0_u8; 1 << 20];
    for _ in 0..40 {
        bomb.write_all(&chunk).unwrap();
    }
    let mut bytes = bomb.finish().unwrap().into_inner();
    bytes[22..26].copy_from_slice(&1_u32.to_le_bytes());
    let central = bytes.windows(4).rposition(|window| window == b"PK\x01\x02").unwrap();
    bytes[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
    upload_refused(&env, bytes, "inflates past").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_ids_never_reach_a_path() {
    let env = env();
    for id in ["../evil", "/etc/evil", "Demo", "ab", "staging", "a/b", "demo.plugin"] {
        let text = format!("id: \"{id}\"\nname: Demo\nversion: 1.0.0\napi_version: 1\n");
        upload_refused(&env, package(&text, &[]), "id").await;
    }
    // a missing manifest is refused too
    upload_refused(&env, zip_with(&[("readme.txt", b"hi")]), "manifest.yml").await;
}

// --- approve ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn approve_refuses_a_hash_or_capability_mismatch_with_409() {
    let env = env();
    let guard = env.system.install_lock().await;
    let caps = "capabilities:\n  network: true\n  storage: true\n";
    env.system.api_upload(&guard, package(&manifest("demo", "1.0.0", caps), &[])).await.unwrap();

    let wrong_hash = PluginApproveDto { wasm_sha256: Some("ab".repeat(32)), ..approve_body(&["network", "storage"]) };
    let rejection = env.system.api_approve(&guard, "demo", &wrong_hash, &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);

    let wrong_sets: [&[&str]; 3] = [&["network"], &["network", "storage", "epg_write"], &[]];
    for wrong in wrong_sets {
        let rejection = env.system.api_approve(&guard, "demo", &approve_body(wrong), &env.config).await.unwrap_err();
        assert_eq!(rejection.status, StatusCode::CONFLICT, "{wrong:?}");
    }
    // nothing was installed or pinned by the refusals
    assert!(!env.plugins.join("demo").exists());
    assert!(!env.data.join("approvals.json").exists());
    assert!(env.data.join("staging/demo").is_dir());

    // the reviewed hash and capabilities approve, in any order
    let summary =
        env.system.api_approve(&guard, "demo", &approve_body(&["storage", "network"]), &env.config).await.unwrap();
    assert_eq!(summary.id, "demo");
    assert_eq!(summary.state, PluginStateDto::Loaded, "approval does not enable");
    assert!(!summary.enabled);
    assert!(env.plugins.join("demo/manifest.yml").is_file());
    assert!(!env.data.join("staging/demo").exists());
    let pins: Value = serde_json::from_str(&fs::read_to_string(env.data.join("approvals.json")).unwrap()).unwrap();
    assert_eq!(pins["demo"]["capabilities"], json!(["network", "storage"]));
    assert_eq!(pins["demo"]["wasm_sha256"], "");
}

#[tokio::test(flavor = "multi_thread")]
async fn approve_without_anything_staged_or_pending_is_404_or_409() {
    let env = env();
    let guard = env.system.install_lock().await;
    let rejection = env.system.api_approve(&guard, "ghost", &approve_body(&[]), &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::NOT_FOUND);
    let rejection = env.system.api_approve(&guard, "../ghost", &approve_body(&[]), &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::NOT_FOUND);

    // an installed plugin that is not waiting for approval
    drop(guard);
    install(&env, "demo", package(&manifest("demo", "1.0.0", ""), &[]), &[]).await;
    let guard = env.system.install_lock().await;
    let rejection = env.system.api_approve(&guard, "demo", &approve_body(&[]), &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_an_update_replaces_the_installed_package_and_reports_what_it_replaces() {
    let env = env();
    install(&env, "demo", package(&manifest("demo", "1.0.0", ""), &[("old.txt", "x")]), &[]).await;

    let guard = env.system.install_lock().await;
    let caps = "capabilities:\n  network: true\n";
    let review = env.system.api_upload(&guard, package(&manifest("demo", "1.1.0", caps), &[])).await.unwrap();
    assert_eq!(review.replaces.as_deref(), Some("1.0.0"));
    let summary = env.system.api_approve(&guard, "demo", &approve_body(&["network"]), &env.config).await.unwrap();
    assert_eq!(summary.version.as_deref(), Some("1.1.0"));
    assert!(!env.plugins.join("demo/old.txt").exists());
    let leftovers: Vec<_> = fs::read_dir(&env.plugins).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(leftovers, ["demo"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hand_placed_unapproved_plugin_can_be_approved_in_place() {
    let env = env();
    let dir = env.plugins.join("manual");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("manifest.yml"), manifest("manual", "0.1.0", "capabilities:\n  storage: true\n")).unwrap();
    env.system.reload(&env.config);
    let guard = env.system.install_lock().await;
    assert_eq!(env.system.api_summary("manual", None).unwrap().state, PluginStateDto::PendingApproval);

    let rejection = env.system.api_approve(&guard, "manual", &approve_body(&[]), &env.config).await.unwrap_err();
    assert_eq!(rejection.status, StatusCode::CONFLICT, "the capabilities do not match");
    let summary = env.system.api_approve(&guard, "manual", &approve_body(&["storage"]), &env.config).await.unwrap();
    assert_eq!(summary.state, PluginStateDto::Loaded);
}

// --- enable -----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn enabling_a_pending_or_invalid_plugin_is_a_409() {
    let env = env();
    let pending = env.plugins.join("pending");
    fs::create_dir_all(&pending).unwrap();
    fs::write(pending.join("manifest.yml"), manifest("pending", "0.1.0", "")).unwrap();
    let broken = env.plugins.join("broken");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("manifest.yml"), "id: broken\nname: [").unwrap();
    env.system.reload(&env.config);

    let guard = env.system.install_lock().await;
    for id in ["pending", "broken"] {
        let rejection = env.system.api_enable_check(&guard, id).unwrap_err();
        assert_eq!(rejection.status, StatusCode::CONFLICT, "{id}");
    }
    assert_eq!(env.system.api_enable_check(&guard, "ghost").unwrap_err().status, StatusCode::NOT_FOUND);

    // once approved, enabling is allowed
    env.system.api_approve(&guard, "pending", &approve_body(&[]), &env.config).await.unwrap();
    env.system.api_enable_check(&guard, "pending").unwrap();
}

// --- uninstall --------------------------------------------------------------

fn write_kv(env: &Env, id: &str) -> PathBuf {
    let dir = env.data.join(id);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("kv.db");
    fs::write(&file, b"stored values").unwrap();
    file
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstall_with_purge_removes_the_kv_store() {
    let env = env();
    install(&env, "demo", package(&manifest("demo", "1.0.0", "capabilities:\n  storage: true\n"), &[]), &["storage"])
        .await;
    let kv = write_kv(&env, "demo");

    let guard = env.system.install_lock().await;
    env.system.api_uninstall(&guard, "demo", true, &env.config).await.unwrap();

    assert!(!env.plugins.join("demo").exists());
    assert!(!kv.exists() && !env.data.join("demo").exists(), "the KV store is gone");
    let pins: Value = serde_json::from_str(&fs::read_to_string(env.data.join("approvals.json")).unwrap()).unwrap();
    assert!(pins.get("demo").is_none(), "the pin is gone");
    assert!(env.system.api_summary("demo", None).is_none());
    assert!(!env.system.api_exists("demo"));
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstall_without_purge_keeps_the_kv_store() {
    let env = env();
    install(&env, "demo", package(&manifest("demo", "1.0.0", ""), &[]), &[]).await;
    let kv = write_kv(&env, "demo");

    let guard = env.system.install_lock().await;
    env.system.api_uninstall(&guard, "demo", false, &env.config).await.unwrap();
    assert!(!env.plugins.join("demo").exists());
    assert!(kv.is_file(), "the data stays unless purge_kv is set");
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstalling_removes_a_staged_copy_and_refuses_unknown_ids() {
    let env = env();
    let guard = env.system.install_lock().await;
    env.system.api_upload(&guard, package(&manifest("demo", "1.0.0", ""), &[])).await.unwrap();
    assert!(env.system.api_exists("demo"));
    env.system.api_uninstall(&guard, "demo", false, &env.config).await.unwrap();
    assert!(!env.data.join("staging/demo").exists());

    for id in ["ghost", "../ghost", "staging"] {
        let rejection = env.system.api_uninstall(&guard, id, true, &env.config).await.unwrap_err();
        assert_eq!(rejection.status, StatusCode::NOT_FOUND, "{id}");
    }
}

// --- ext.<section> settings -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ext_sections_are_validated_against_their_fragments() {
    let env = env();
    install(&env, "demo", package(&manifest("demo", "1.0.0", EXTENSION), &[]), &[]).await;
    let config = PluginsConfig { plugins: vec![entry_with("demo", json!({}))], ..env.config.clone() };

    let ok = json!({"ext": {"proxy": {"port": 8080}}});
    env.system.api_prepare_settings("demo", &ok, Some(&config)).await.unwrap();

    let wrong_type = json!({"ext": {"proxy": {"port": "eighty"}}});
    let (status, error) = env.system.api_prepare_settings("demo", &wrong_type, Some(&config)).await.unwrap_err();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error.fields.len(), 1);
    assert_eq!(error.fields[0].path, "ext.proxy.port");

    let unknown_section = json!({"ext": {"webhooks": {"x": 1}}});
    let (status, error) = env.system.api_prepare_settings("demo", &unknown_section, Some(&config)).await.unwrap_err();
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(error.error.contains("ext.webhooks"), "{}", error.error);

    for not_an_object in [json!({"ext": "nope"}), json!({"ext": {"proxy": 5}})] {
        let (status, _) = env.system.api_prepare_settings("demo", &not_an_object, Some(&config)).await.unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ext_secrets_are_masked_kept_on_the_sentinel_and_warned_about() {
    let env = env();
    install(&env, "demo", package(&manifest("demo", "1.0.0", EXTENSION), &[]), &[]).await;
    let stored = json!({"ext": {"proxy": {"port": 1, "key": "s3cret"}}});
    let config = PluginsConfig { plugins: vec![entry_with("demo", stored)], ..env.config.clone() };

    let shown = env.system.api_settings("demo", Some(&config)).unwrap();
    assert_eq!(shown.settings["ext"]["proxy"]["key"], REDACTED_SECRET);
    assert_eq!(shown.settings["ext"]["proxy"]["port"], 1);
    assert!(shown.secrets.iter().any(|secret| secret.path == "ext.proxy.key" && secret.set));
    assert!(!shown.settings.to_string().contains("s3cret"));

    // echoing the sentinel keeps the stored value
    let echoed = json!({"ext": {"proxy": {"port": 2, "key": REDACTED_SECRET}}});
    let (saved, warnings) = env.system.api_prepare_settings("demo", &echoed, Some(&config)).await.unwrap();
    assert_eq!(saved["ext"]["proxy"]["key"], "s3cret");
    assert!(warnings.iter().all(|warning| !warning.contains("s3cret")));

    // a new literal is stored, with a warning that names the path and not the value
    let literal = json!({"ext": {"proxy": {"key": "fresh-literal"}}});
    let (_saved, warnings) = env.system.api_prepare_settings("demo", &literal, Some(&config)).await.unwrap();
    assert!(warnings.iter().any(|warning| warning.contains("ext.proxy.key")), "{warnings:?}");
    assert!(warnings.iter().all(|warning| !warning.contains("fresh-literal")));
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_of_a_plugin_without_extensions_treat_ext_as_an_ordinary_key() {
    let env = env();
    install(&env, "demo", package(&manifest("demo", "1.0.0", ""), &[]), &[]).await;
    let incoming = json!({"ext": {"anything": true}});
    let (saved, _) = env.system.api_prepare_settings("demo", &incoming, Some(&env.config)).await.unwrap();
    assert_eq!(saved["ext"], json!({"anything": true}));
}
