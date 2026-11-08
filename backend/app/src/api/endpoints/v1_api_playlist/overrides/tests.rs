//! Tests of the override API.
//!
//! The tests mount the production router registrations from `routes.rs` (both the permission-split
//! `v1_api_playlist_register_with_permissions` and the auth-disabled `v1_api_playlist_register_protected`),
//! so the handlers, the permission split and the actual route table are exercised over `oneshot` requests.

use super::super::routes::{v1_api_playlist_register_protected, v1_api_playlist_register_with_permissions};
use crate::{
    api::model::{create_test_app_state, AppState},
    auth::create_jwt_web_user,
    model::{
        Config, ConfigInput, ConfigInputAlias, ConfigSource, ConfigTarget, M3uTargetOutput, SourcesConfig,
        TargetOutput, XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    repository::{
        build_input_storage_path, encode_override_file_name, get_input_m3u_playlist_file_path, get_target_storage_path,
        m3u_get_epg_file_path_for_target, m3u_get_file_path_for_db, xtream_get_file_path, BPlusTree, OverrideRevision,
        OverrideScope,
    },
};
use arc_swap::ArcSwapOption;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use shared::{
    foundation::Filter,
    model::{
        ConfigPaths, ConfigTargetOptions, EpgChannel, EpgConfigDto, EpgOutputOptions, EpgSourceDto, InputType,
        M3uPlaylistItem, Permission, PlaylistItemType, ProcessingOrder, UiPlaylistItem, UserId, VirtualId,
        WebAuthConfigDto, WebUiConfigDto, XtreamCluster, XtreamPlaylistItem,
    },
    utils::{extract_id_from_url, Internable},
};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tower::ServiceExt;

const CONFIG_INPUT: &str = "m3u-input";
const OTHER_INPUT: &str = "xtream-input";
const M3U_TARGET: &str = "m3u-target";
const XTREAM_TARGET: &str = "xtream-target";

// ---------------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------------

struct Fixture {
    dir: tempfile::TempDir,
    app_state: Arc<AppState>,
    /// The production router with the permission split (authentication enabled).
    router: Router,
    /// The production router used when authentication is disabled.
    public_router: Router,
    config_dir: PathBuf,
    storage_dir: PathBuf,
    reader: String,
    writer: String,
    epg_only: String,
}

fn m3u_output() -> TargetOutput {
    TargetOutput::M3u(M3uTargetOutput {
        filename: None,
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
    })
}

fn xtream_output() -> TargetOutput {
    TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None })
}

fn target(id: u16, name: &str, output: TargetOutput) -> Arc<ConfigTarget> {
    Arc::new(ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id,
        enabled: true,
        name: name.to_string(),
        options: Some(ConfigTargetOptions {
            epg_output: EpgOutputOptions { lowercase_ids: true, ..EpgOutputOptions::default() },
            ..ConfigTargetOptions::default()
        }),
        sort: None,
        filter: Filter::default().into(),
        output: vec![output],
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::new(None)),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: false,
    })
}

fn input(id: u16, name: &str, input_type: InputType) -> Arc<ConfigInput> {
    Arc::new(ConfigInput {
        id,
        name: name.intern(),
        input_type,
        url: "http://provider.example/get.php?username=secret-user&password=secret-pass".to_string(),
        username: Some("secret-user".to_string()),
        password: Some("secret-pass".to_string()),
        ..ConfigInput::default()
    })
}

/// Runtime EPG configuration of one XMLTV source; the cache path derives from it.
fn epg_config(url: &str) -> crate::model::EpgConfig {
    let source = EpgSourceDto { url: url.to_string(), priority: 0, ..EpgSourceDto::default() };
    let dto = EpgConfigDto { sources: Some(vec![source.clone()]), smart_match: None, t_sources: vec![source] };
    crate::model::EpgConfig::from(&dto)
}

fn input_with_epg(id: u16, name: &str, input_type: InputType, epg_url: &str) -> Arc<ConfigInput> {
    Arc::new(ConfigInput { epg: Some(epg_config(epg_url)), ..(*input(id, name, input_type)).clone() })
}

fn alias_input(id: u16, name: &str, alias: &str) -> Arc<ConfigInput> {
    let aliases = vec![ConfigInputAlias {
        id,
        name: alias.intern(),
        url: "http://provider.example/get.php?username=secret-user&password=secret-pass".to_string(),
        username: Some("secret-user".to_string()),
        password: Some("secret-pass".to_string()),
        priority: 0,
        max_connections: 1,
        exp_date: None,
        enabled: true,
        stalker: None,
    }];
    Arc::new(ConfigInput { aliases: Some(aliases), ..(*input(id, name, InputType::M3u)).clone() })
}

/// A minimal but valid XMLTV document whose programmes sit around now, as the web-ui parser keeps only
/// channels with programmes in that window.
fn xmltv_cache(channels: &[(&str, &str)]) -> String {
    let start = (chrono::Utc::now() - chrono::Duration::hours(1)).format("%Y%m%d%H%M%S %z").to_string();
    let stop = (chrono::Utc::now() + chrono::Duration::hours(2)).format("%Y%m%d%H%M%S %z").to_string();
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<tv>\n");
    for (id, title) in channels {
        let _ = std::fmt::Write::write_fmt(
            &mut body,
            format_args!(
                "  <channel id=\"{id}\"><display-name>{title}</display-name></channel>\n  \
                 <programme start=\"{start}\" stop=\"{stop}\" channel=\"{id}\"><title>{title}</title></programme>\n"
            ),
        );
    }
    body.push_str("</tv>\n");
    body
}

fn config_paths(config_dir: &std::path::Path, storage_dir: &std::path::Path) -> ConfigPaths {
    ConfigPaths {
        home_path: String::new(),
        config_path: config_dir.to_string_lossy().into_owned(),
        storage_path: storage_dir.to_string_lossy().into_owned(),
        config_file_path: String::new(),
        sources_file_path: String::new(),
        mapping_file_path: None,
        mapping_files_used: None,
        template_file_path: None,
        template_files_used: None,
        api_proxy_file_path: String::new(),
        custom_stream_response_path: None,
    }
}

/// The routes under test: the production registrations, not a replica of them.
fn test_router(app_state: &Arc<AppState>) -> Router {
    v1_api_playlist_register_with_permissions(Router::new(), app_state).with_state(Arc::clone(app_state))
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path().to_path_buf();
    let config_dir = root.join("config");
    let storage_dir = root.join("storage");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::create_dir_all(&storage_dir).expect("storage dir");

    let web_ui = WebUiConfigDto {
        auth: Some(WebAuthConfigDto {
            enabled: true,
            issuer: "override-test".to_owned(),
            secret: "override-test-secret".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = Config {
        web_ui: Some((&web_ui).into()),
        storage_dir: storage_dir.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let web_auth = config.web_ui.as_ref().and_then(|ui| ui.auth.as_ref()).expect("web auth");
    let token = |name: &str, permission: Permission| {
        create_jwt_web_user(web_auth, name, permission.into(), 0, UserId::from(format!("web:{name}").as_str()))
            .expect("jwt")
    };
    let reader = token("reader", Permission::PlaylistRead);
    let writer = token("writer", Permission::PlaylistWrite);
    let epg_only = token("epg_only", Permission::EpgRead);

    let app_state = create_test_app_state(config);
    app_state.app_config.paths.store(Arc::new(config_paths(&config_dir, &storage_dir)));

    let inputs = vec![input(1, CONFIG_INPUT, InputType::M3u), input(2, OTHER_INPUT, InputType::Xtream)];
    let sources = SourcesConfig {
        group_lookup: shared::model::provider_saturation::build_group_lookup(&inputs),
        inputs,
        sources: vec![ConfigSource {
            inputs: vec![CONFIG_INPUT.intern(), OTHER_INPUT.intern()],
            targets: vec![target(1, M3U_TARGET, m3u_output()), target(2, XTREAM_TARGET, xtream_output())],
        }],
        ..SourcesConfig::default()
    };
    app_state.app_config.sources.store(Arc::new(sources));

    let router = test_router(&app_state);
    let public_router = v1_api_playlist_register_protected(Router::new()).with_state(Arc::clone(&app_state));
    Fixture { dir, app_state, router, public_router, config_dir, storage_dir, reader, writer, epg_only }
}

impl Fixture {
    /// Replaces the configured inputs and source blocks of the fixture.
    fn install_sources(&self, inputs: Vec<Arc<ConfigInput>>, sources: Vec<ConfigSource>) {
        let group_lookup = shared::model::provider_saturation::build_group_lookup(&inputs);
        self.app_state.app_config.sources.store(Arc::new(SourcesConfig {
            group_lookup,
            inputs,
            sources,
            ..SourcesConfig::default()
        }));
    }

    fn file_path(&self, scope: &OverrideScope) -> PathBuf {
        self.app_state.override_repository().file_path(scope).expect("override file path")
    }

    fn input_file(&self, name: &str) -> PathBuf { self.file_path(&OverrideScope::Input(name.to_string())) }

    fn target_file(&self, name: &str) -> PathBuf { self.file_path(&OverrideScope::Target(name.to_string())) }

    fn input_storage(&self, name: &str) -> PathBuf {
        build_input_storage_path(name, &self.storage_dir.to_string_lossy())
    }

    async fn call(&self, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        self.call_on(&self.router, method, uri, token, body).await
    }

    /// A request to the auth-disabled registration: no token is sent and none is expected.
    async fn call_unauthenticated(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_on(&self.public_router, method, uri, None, body).await
    }

    async fn call_on(
        &self,
        router: &Router,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&body).expect("json body"))
            }
            None => Body::empty(),
        };
        let response = router.clone().oneshot(request.body(body).expect("request")).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) { self.call("GET", uri, Some(&self.reader), None).await }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.call("POST", uri, Some(&self.writer), Some(body)).await
    }

    async fn revision(&self, scope: &str, name: &str) -> String {
        let (status, body) = self.get(&format!("/playlist/overrides?scope={scope}&name={name}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["revision"].as_str().expect("revision").to_string()
    }

    /// Stored records of an input, as a production input load would leave them.
    fn write_input_playlist(&self, name: &str, items: Vec<M3uPlaylistItem>) {
        let storage = self.input_storage(name);
        std::fs::create_dir_all(&storage).expect("input storage");
        let path = get_input_m3u_playlist_file_path(&storage, &name.intern());
        let mut tree = BPlusTree::<Arc<str>, M3uPlaylistItem>::new();
        for item in items {
            tree.insert(Arc::clone(&item.provider_id), item);
        }
        store(&path, tree);
    }

    fn write_xtream_input_cluster(&self, name: &str, cluster: XtreamCluster, items: Vec<XtreamPlaylistItem>) {
        let storage = self.input_storage(name);
        std::fs::create_dir_all(&storage).expect("input storage");
        let path = xtream_get_file_path(&storage, cluster);
        let mut tree = BPlusTree::<u32, XtreamPlaylistItem>::new();
        for item in items {
            tree.insert(item.virtual_id.get(), item);
        }
        store(&path, tree);
    }

    fn write_xtream_input_playlist(&self, name: &str, items: Vec<XtreamPlaylistItem>) {
        self.write_xtream_input_cluster(name, XtreamCluster::Live, items);
    }

    fn write_input_status(&self, name: &str, state: crate::processing::input_cache::ClusterState) {
        use crate::processing::input_cache::{ClusterStatus, InputStatus, STATUS_FILE};
        let storage = self.input_storage(name);
        std::fs::create_dir_all(&storage).expect("input storage");
        let mut clusters = HashMap::new();
        clusters.insert("live".to_string(), ClusterStatus { status: state, timestamp: 1, last_update: None });
        let status = InputStatus { clusters, last_input_update: None, epg_sources: Vec::new() };
        std::fs::write(storage.join(STATUS_FILE), serde_json::to_vec(&status).expect("status json"))
            .expect("status written");
    }

    fn write_input_status_with_clusters(&self, name: &str, cluster_names: &[&str]) {
        use crate::processing::input_cache::{ClusterStatus, InputStatus, STATUS_FILE};
        let storage = self.input_storage(name);
        std::fs::create_dir_all(&storage).expect("input storage");
        let mut clusters = HashMap::new();
        for &c in cluster_names {
            clusters.insert(
                c.to_string(),
                ClusterStatus {
                    status: crate::processing::input_cache::ClusterState::Ok,
                    timestamp: 1,
                    last_update: None,
                },
            );
        }
        let status = InputStatus { clusters, last_input_update: None, epg_sources: Vec::new() };
        std::fs::write(storage.join(STATUS_FILE), serde_json::to_vec(&status).expect("status json"))
            .expect("status written");
    }

    fn write_target_playlist(&self, name: &str, items: Vec<M3uPlaylistItem>) {
        let config = self.app_state.app_config.config.load_full();
        let target_path = get_target_storage_path(&config, name).expect("target path");
        let path = m3u_get_file_path_for_db(&target_path);
        let mut tree = BPlusTree::<u32, M3uPlaylistItem>::new();
        for item in items {
            tree.insert(item.virtual_id.get(), item);
        }
        store(&path, tree);
    }

    fn write_xtream_target_playlist(&self, name: &str, items: Vec<XtreamPlaylistItem>) {
        let config = self.app_state.app_config.config.load_full();
        let target_path = crate::repository::xtream_get_storage_path(&config, name).expect("target path");
        let path = xtream_get_file_path(&target_path, XtreamCluster::Live);
        let mut tree = BPlusTree::<u32, XtreamPlaylistItem>::new();
        for item in items {
            tree.insert(item.virtual_id.get(), item);
        }
        store(&path, tree);
    }

    fn write_target_epg(&self, name: &str, ids: &[(&str, &str)]) {
        let config = self.app_state.app_config.config.load_full();
        let target_path = get_target_storage_path(&config, name).expect("target path");
        let path = m3u_get_epg_file_path_for_target(&target_path);
        let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
        for (id, title) in ids {
            let channel =
                EpgChannel { id: id.intern(), title: Some(title.intern()), icon: None, programmes: Vec::new() };
            tree.insert(Arc::clone(&channel.id), channel);
        }
        store(&path, tree);
    }

    /// The raw EPG cache file path of the first EPG source of `input`.
    async fn raw_epg_path(&self, input: &Arc<ConfigInput>) -> PathBuf {
        let source = input.epg.as_ref().and_then(|epg| epg.sources.first()).expect("an EPG source");
        crate::processing::epg::get_input_raw_epg_file_path(source, input, &self.storage_dir.to_string_lossy())
            .await
            .expect("raw epg path")
    }
}

fn write_override_file(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("override dir");
    std::fs::write(path, content).expect("override file");
}

fn store<K, V>(path: &std::path::Path, mut tree: BPlusTree<K, V>)
where
    K: Ord + Clone + serde::Serialize + for<'de> serde::Deserialize<'de>,
    V: Clone + serde::Serialize + for<'de> serde::Deserialize<'de>,
{
    std::fs::create_dir_all(path.parent().expect("parent")).expect("storage dir");
    tree.store(path).expect("db written");
}

fn m3u_record(provider_id: &str, name: &str, group: &str, item_type: PlaylistItemType) -> M3uPlaylistItem {
    m3u_record_of(CONFIG_INPUT, provider_id, name, group, item_type)
}

fn m3u_record_of(
    input_name: &str,
    provider_id: &str,
    name: &str,
    group: &str,
    item_type: PlaylistItemType,
) -> M3uPlaylistItem {
    M3uPlaylistItem {
        virtual_id: VirtualId::new(provider_id.parse::<u32>().unwrap_or(1)),
        provider_id: provider_id.intern(),
        name: name.intern(),
        chno: 3,
        logo: "http://logo.example/a.png".intern(),
        logo_small: "".intern(),
        group: group.intern(),
        title: name.intern(),
        parent_code: "".intern(),
        audio_track: "".intern(),
        time_shift: "".intern(),
        rec: "".intern(),
        url: format!("http://provider.example/live/secret-user/secret-pass/{provider_id}.ts").intern(),
        epg_channel_id: None,
        input_name: input_name.intern(),
        item_type,
        t_stream_url: "".intern(),
        t_resource_url: None,
        t_catchup_source: None,
        t_catchup_mode: None,
        source_ordinal: 0,
        additional_properties: None,
        input_stream_id: provider_id.intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    }
}

fn xtream_record(virtual_id: u32, provider_id: u32, name: &str, group: &str, input_name: &str) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(virtual_id),
        provider_id,
        name: name.intern(),
        logo: "http://logo.example/a.png".intern(),
        logo_small: "".intern(),
        group: group.intern(),
        title: name.intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: format!("http://provider.example/live/secret-user/secret-pass/{provider_id}.ts").intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Live,
        additional_properties: None,
        item_type: PlaylistItemType::Live,
        category_id: 1,
        input_name: input_name.intern(),
        channel_no: 3,
        source_ordinal: 0,
        input_stream_id: provider_id.to_string().intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    }
}

fn patch(input: Option<&str>, kind: &str, id: &str, patch: Value) -> Value {
    let mut value = json!({ "kind": kind, "id": id });
    value["patch"] = patch;
    if let Some(input) = input {
        value["input"] = json!(input);
    }
    value
}

fn save_body(scope: &str, name: &str, base: &str, patches: Value) -> Value {
    let mut value = json!({ "scope": scope, "name": name, "base_revision": base });
    value["patches"] = patches;
    value
}

fn set(value: &str) -> Value { json!({ "op": "set", "value": value }) }

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn write_requires_playlist_write() {
    let fixture = fixture();
    let base = fixture.revision("target", M3U_TARGET).await;
    let body = save_body(
        "target",
        M3U_TARGET,
        &base,
        json!([patch(Some(CONFIG_INPUT), "live", "p1", json!({ "group": set("News") }))]),
    );

    let (status, _) = fixture.call("POST", "/playlist/overrides", Some(&fixture.reader), Some(body.clone())).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a read-only token may not write overrides");

    let (status, _) = fixture.call("POST", "/playlist/overrides", None, Some(body.clone())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "an anonymous request may not write overrides");

    let (status, body) = fixture.call("POST", "/playlist/overrides", Some(&fixture.writer), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["revision"].as_str().is_some_and(|revision| !revision.is_empty()));

    let discard = json!({
        "scope": "target",
        "name": M3U_TARGET,
        "base_revision": base,
        "items": [{ "input": CONFIG_INPUT, "kind": "live", "id": "p1" }]
    });
    let (status, _) = fixture.call("POST", "/playlist/overrides/discard", Some(&fixture.reader), Some(discard)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "discard is a write as well");

    for uri in ["/playlist/overrides?scope=input&name=m3u-input", "/playlist/overrides/orphans?input=m3u-input"] {
        let (status, _) = fixture.call("GET", uri, Some(&fixture.epg_only), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri} requires PlaylistRead");
    }
}

#[tokio::test]
async fn save_conflict_returns_409() {
    let fixture = fixture();
    fixture.write_input_playlist(CONFIG_INPUT, vec![m3u_record("p1", "One", "G", PlaylistItemType::Live)]);
    let base = fixture.revision("input", CONFIG_INPUT).await;

    // An external editor replaces the file after the client read it.
    write_override_file(&fixture.input_file(CONFIG_INPUT), "schema_version: 1\nlive:\n  p1:\n    group: Other\n");

    let body =
        save_body("input", CONFIG_INPUT, &base, json!([patch(None, "live", "p1", json!({ "name": set("Renamed") }))]));
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let current = body["current_revision"].as_str().expect("current revision");
    assert_ne!(current, base, "the conflict reports the revision the file has now");
    assert_eq!(current, fixture.revision("input", CONFIG_INPUT).await);

    // Nothing was overwritten: the external change is still the file's content.
    let stored = std::fs::read_to_string(fixture.input_file(CONFIG_INPUT)).expect("file");
    assert!(stored.contains("Other"), "{stored}");
    assert!(!stored.contains("Renamed"), "{stored}");

    // The same base revision now succeeds.
    let body = save_body(
        "input",
        CONFIG_INPUT,
        current,
        json!([patch(None, "live", "p1", json!({ "name": set("Renamed") }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn invalid_value_returns_422_field_error() {
    let fixture = fixture();
    let base = fixture.revision("input", CONFIG_INPUT).await;

    // `tmdb` is a movie field, not a live field.
    let body = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([patch(None, "live", "p1", json!({ "tmdb": json!({ "op": "set", "value": 42 }) }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let errors = body["field_errors"].as_array().expect("field errors");
    assert!(errors.iter().any(|error| error["field"] == "tmdb" && error["message_key"] == "error.field_not_allowed"));

    // A logo has to be an http(s) URL.
    let body = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([patch(None, "live", "p1", json!({ "logo": set("not-a-url") }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let errors = body["field_errors"].as_array().expect("field errors");
    assert!(errors.iter().any(|error| error["field"] == "logo"));

    // Without a field error the same patch is accepted.
    let body = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([patch(None, "live", "p1", json!({ "logo": set("https://logo.example/a.png") }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn path_traversal_name_rejected_or_encoded() {
    let fixture = fixture();
    let base = fixture.revision("target", M3U_TARGET).await;
    let hostile = "../../etc/passwd";

    let (status, _) = fixture.get(&format!("/playlist/overrides?scope=target&name={hostile}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "an unknown name is not disclosed");

    let body = save_body(
        "target",
        hostile,
        &base,
        json!([patch(Some(CONFIG_INPUT), "live", "p1", json!({ "name": set("x") }))]),
    );
    let (status, _) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The name never becomes a path: it is percent-encoded inside the override directory.
    let path = fixture.target_file(hostile);
    let overrides = fixture.config_dir.join("overrides");
    assert!(path.starts_with(&overrides), "{} escapes {}", path.display(), overrides.display());
    let encoded = encode_override_file_name(hostile).expect("encoded");
    assert_eq!(path.file_name().and_then(|name| name.to_str()), Some(format!("{encoded}.yml").as_str()));
    assert!(!encoded.contains('/'), "{encoded}");
    assert!(!path.exists(), "a rejected request writes nothing");

    // Nothing escaped next to the configuration directory either.
    assert!(!fixture.dir.path().join("etc").exists());
}

#[tokio::test]
async fn orphan_filtered_item_is_active() {
    let fixture = fixture();
    fixture.write_input_playlist(
        CONFIG_INPUT,
        vec![
            m3u_record("p1", "One", "G", PlaylistItemType::Live),
            m3u_record("p2", "Two", "G", PlaylistItemType::Live),
        ],
    );
    fixture.write_input_status(CONFIG_INPUT, crate::processing::input_cache::ClusterState::Ok);
    // The target playlist lost `p1` to a filter; the input inventory still has it.
    fixture.write_target_playlist(M3U_TARGET, vec![m3u_record("p2", "Two", "G", PlaylistItemType::Live)]);

    let base = fixture.revision("input", CONFIG_INPUT).await;
    let body = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([
            patch(None, "live", "p1", json!({ "group": set("News") })),
            patch(None, "live", "p9", json!({ "group": set("Gone") })),
        ]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={CONFIG_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["entries"].as_array().expect("entries");
    let state_of = |id: &str| {
        entries
            .iter()
            .find(|entry| entry["id"] == id)
            .and_then(|entry| entry["state"].as_str())
            .unwrap_or("missing")
            .to_string()
    };
    assert_eq!(state_of("p1"), "active", "a filtered item is not an orphan: {body}");
    assert_eq!(state_of("p9"), "orphaned", "{body}");
}

#[tokio::test]
async fn orphan_failed_input_is_unknown() {
    let fixture = fixture();
    fixture.write_input_playlist(CONFIG_INPUT, vec![]);
    fixture.write_input_status(CONFIG_INPUT, crate::processing::input_cache::ClusterState::Failed);

    let base = fixture.revision("input", CONFIG_INPUT).await;
    let body =
        save_body("input", CONFIG_INPUT, &base, json!([patch(None, "live", "p1", json!({ "group": set("News") }))]));
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={CONFIG_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["state"], "unknown", "a failed input update never marks entries orphaned: {body}");

    // No stored data at all is unknown as well.
    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={OTHER_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["entries"].as_array().is_none_or(Vec::is_empty), "{body}");
}

#[tokio::test]
async fn orphan_scan_of_unscanned_input_type_is_unknown() {
    const STALKER_INPUT: &str = "stalker-input";
    let fixture = fixture();
    fixture.install_sources(
        vec![input(3, STALKER_INPUT, InputType::Stalker)],
        vec![ConfigSource { inputs: vec![STALKER_INPUT.intern()], targets: vec![target(1, M3U_TARGET, m3u_output())] }],
    );
    fixture.write_input_status(STALKER_INPUT, crate::processing::input_cache::ClusterState::Ok);

    let base = fixture.revision("input", STALKER_INPUT).await;
    let body =
        save_body("input", STALKER_INPUT, &base, json!([patch(None, "live", "s1", json!({ "name": set("Kept") }))]));
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={STALKER_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["entries"].as_array().expect("entries");
    assert_eq!(entries.len(), 1, "{body}");
    assert_eq!(entries[0]["state"], "unknown", "no inventory was read, so nothing is orphaned: {body}");
}

#[tokio::test]
async fn orphan_scan_skips_absent_xtream_clusters() {
    let fixture = fixture();
    // OTHER_INPUT is an Xtream input. Only live cluster is written to disk and recorded in status.
    fixture.write_xtream_input_playlist(OTHER_INPUT, vec![xtream_record(1, 42, "Live One", "Live Group", OTHER_INPUT)]);
    fixture.write_input_status(OTHER_INPUT, crate::processing::input_cache::ClusterState::Ok);

    let base = fixture.revision("input", OTHER_INPUT).await;
    let body = save_body(
        "input",
        OTHER_INPUT,
        &base,
        json!([
            patch(None, "live", "42", json!({ "name": set("Edited") })),
            patch(None, "live", "77", json!({ "name": set("Gone") })),
            patch(None, "series", "99", json!({ "name": set("Unscanned") })),
        ]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let state_of = |body: &Value, id: &str| {
        body["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .find(|entry| entry["id"] == id)
            .map_or_else(|| panic!("entry {id} missing: {body}"), |entry| entry["state"].clone())
    };

    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={OTHER_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The absent series cluster does not fail the scan of the recorded live cluster ...
    assert_eq!(state_of(&body, "42"), "active", "{body}");
    assert_eq!(state_of(&body, "77"), "orphaned", "{body}");
    // ... but its entries cannot be proven orphaned.
    assert_eq!(state_of(&body, "99"), "unknown", "{body}");

    // When a cluster IS recorded in status (e.g. series: Ok) but its file is missing,
    // the existing failure behavior is preserved (entry state becomes unknown).
    fixture.write_input_status_with_clusters(OTHER_INPUT, &["live", "series"]);
    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={OTHER_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for id in ["42", "77", "99"] {
        assert_eq!(
            state_of(&body, id),
            "unknown",
            "a recorded cluster whose file is missing must fail the scan: {body}"
        );
    }
}

#[tokio::test]
async fn renamed_input_lists_orphaned_file() {
    let fixture = fixture();
    fixture.write_input_status(CONFIG_INPUT, crate::processing::input_cache::ClusterState::Ok);
    // The file of an input that no longer exists under that name.
    write_override_file(&fixture.input_file("old-input"), "schema_version: 1\nlive:\n  p1:\n    group: News\n");
    write_override_file(&fixture.target_file(M3U_TARGET), "schema_version: 1\n");

    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={CONFIG_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let files = body["orphaned_files"].as_array().expect("orphaned files");
    let names: Vec<&str> = files.iter().filter_map(|file| file["name"].as_str()).collect();
    assert!(names.contains(&"old-input"), "{body}");
    assert!(!names.contains(&M3U_TARGET), "a configured target is not orphaned: {body}");
    assert!(!names.contains(&CONFIG_INPUT), "a configured input is not orphaned: {body}");
    let orphan = files.iter().find(|file| file["name"] == "old-input").expect("orphan entry");
    assert_eq!(orphan["scope"], "input");
    assert!(orphan["revision"].as_str().is_some_and(|revision| !revision.is_empty()));
}

#[tokio::test]
async fn epg_search_bounded() {
    let fixture = fixture();
    fixture.write_target_epg(
        M3U_TARGET,
        &[("news-a", "News A"), ("news-b", "News B"), ("xnews", "Extra News"), ("sports", "Sports")],
    );

    let (status, body) = fixture.get("/playlist/epg/search?target_id=1&q=news&limit=10").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body.as_array().expect("array").iter().filter_map(|entry| entry["id"].as_str()).collect();
    assert_eq!(ids, vec!["news-a", "news-b", "xnews"], "prefix matches rank first, substring matches follow: {body}");
    assert_eq!(body[0]["display_name"], "News A");

    // The limit is clamped, never exceeded.
    let many: Vec<(String, String)> =
        (0..80).map(|index| (format!("news-{index:02}"), format!("News {index:02}"))).collect();
    let many_ids: Vec<(&str, &str)> = many.iter().map(|(id, title)| (id.as_str(), title.as_str())).collect();
    fixture.write_target_epg(M3U_TARGET, &many_ids);
    let (status, body) = fixture.get("/playlist/epg/search?target_id=1&q=news&limit=500").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().expect("array").len(), 50, "the maximum is 50 results");

    let (status, body) = fixture.get("/playlist/epg/search?target_id=1&q=news&limit=3").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().expect("array").len(), 3);

    // More matches than the collection bound: the scan stops at the cap and the answer stays bounded.
    let capped: Vec<(String, String)> = (0..(super::EPG_SEARCH_MAX_MATCHES + 100))
        .map(|index| (format!("news-{index:04}"), format!("News {index:04}")))
        .collect();
    let capped_ids: Vec<(&str, &str)> = capped.iter().map(|(id, title)| (id.as_str(), title.as_str())).collect();
    fixture.write_target_epg(M3U_TARGET, &capped_ids);
    let (status, body) = fixture.get("/playlist/epg/search?target_id=1&q=news&limit=50").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().expect("array").len(), 50, "the answer stays bounded: {body}");

    let (status, _) = fixture.get("/playlist/epg/search?target_id=1&limit=3").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "'q' is required");
    let (status, _) = fixture.get("/playlist/epg/search?target_id=1&input=m3u-input&q=news").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "exactly one source is required");
}

#[tokio::test]
async fn request_validation_cases() {
    let fixture = fixture();
    let base = fixture.revision("input", CONFIG_INPUT).await;

    let empty = save_body("input", CONFIG_INPUT, &base, json!([]));
    let (status, _) = fixture.post("/playlist/overrides", empty).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an empty patch list is rejected");

    let duplicate = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([
            patch(None, "live", "p1", json!({ "group": set("A") })),
            patch(None, "live", "p1", json!({ "title": set("B") })),
        ]),
    );
    let (status, _) = fixture.post("/playlist/overrides", duplicate).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "duplicate (input, kind, id) is rejected");

    let mixed = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([patch(Some(OTHER_INPUT), "live", "p1", json!({ "group": set("A") }))]),
    );
    let (status, _) = fixture.post("/playlist/overrides", mixed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an input file never holds another input");

    let missing_input =
        save_body("target", M3U_TARGET, &base, json!([patch(None, "live", "p1", json!({ "group": set("A") }))]));
    let (status, _) = fixture.post("/playlist/overrides", missing_input).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a target patch names its input");

    let bad_scope =
        save_body("playlist", CONFIG_INPUT, &base, json!([patch(None, "live", "p1", json!({ "group": set("A") }))]));
    let (status, _) = fixture.post("/playlist/overrides", bad_scope).await;
    // A closed-set enum is rejected by the body deserializer before the handler runs.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "an unknown scope is rejected");
    let bad_scope_get = fixture.get(&format!("/playlist/overrides?scope=playlist&name={CONFIG_INPUT}")).await;
    assert_eq!(bad_scope_get.0, StatusCode::BAD_REQUEST, "the read endpoint lists the allowed scopes");

    let blank_id =
        save_body("input", CONFIG_INPUT, &base, json!([patch(None, "live", "  ", json!({ "group": set("A") }))]));
    let (status, _) = fixture.post("/playlist/overrides", blank_id).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an empty id is rejected");

    // JSON null is not a field operation: the delta format has tags, `null` is not one of them.
    let null_patch =
        save_body("input", CONFIG_INPUT, &base, json!([patch(None, "live", "p1", json!({ "group": null }))]));
    let (status, _) = fixture.post("/playlist/overrides", null_patch).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "JSON null is rejected");

    let null_value = save_body(
        "input",
        CONFIG_INPUT,
        &base,
        json!([patch(None, "live", "p1", json!({ "group": { "op": "set", "value": null } }))]),
    );
    let (status, _) = fixture.post("/playlist/overrides", null_value).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "a null value is rejected");

    let unknown_kind =
        save_body("input", CONFIG_INPUT, &base, json!([patch(None, "radio", "p1", json!({ "group": set("A") }))]));
    let (status, _) = fixture.post("/playlist/overrides", unknown_kind).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "an unknown kind does not deserialize");

    // Nothing was written by any of the rejected requests.
    assert!(!fixture.input_file(CONFIG_INPUT).exists());
}

#[tokio::test]
async fn hidden_target_not_disclosed() {
    let fixture = fixture();

    // Without the permission, an existing and an unknown name answer alike: nothing leaks.
    let existing = fixture
        .call("GET", &format!("/playlist/overrides?scope=target&name={M3U_TARGET}"), Some(&fixture.epg_only), None)
        .await;
    let unknown = fixture
        .call("GET", "/playlist/overrides?scope=target&name=no-such-target", Some(&fixture.epg_only), None)
        .await;
    assert_eq!(existing.0, StatusCode::FORBIDDEN);
    assert_eq!(anonymous_body(&existing), anonymous_body(&unknown));

    // With the permission, an unknown target and an unknown input answer with the same shape.
    let (status, target_body) = fixture.get("/playlist/overrides?scope=target&name=no-such-target").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(target_body, json!({ "error": "Unknown target 'no-such-target'" }));
    let (status, input_body) = fixture.get("/playlist/overrides?scope=input&name=no-such-input").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(input_body, json!({ "error": "Unknown input 'no-such-input'" }));

    let (status, body) = fixture.get(&format!("/playlist/overrides?scope=target&name={M3U_TARGET}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["entries"], json!([]));
}

fn anonymous_body(response: &(StatusCode, Value)) -> String { response.1.to_string() }

#[tokio::test]
async fn target_patch_never_touches_input_file() {
    let fixture = fixture();
    fixture.write_input_playlist(CONFIG_INPUT, vec![m3u_record("p1", "One", "Group", PlaylistItemType::Live)]);

    let base = fixture.revision("target", M3U_TARGET).await;
    let body = save_body(
        "target",
        M3U_TARGET,
        &base,
        json!([patch(Some(CONFIG_INPUT), "live", "p1", json!({ "name": set("Target Name") }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let target_file = fixture.target_file(M3U_TARGET);
    let stored = std::fs::read_to_string(&target_file).expect("target file");
    assert!(stored.contains(CONFIG_INPUT), "{stored}");
    assert!(stored.contains("Target Name"), "{stored}");
    assert!(!fixture.input_file(CONFIG_INPUT).exists(), "a target patch never writes an input file");

    // The hint was taken from the raw input item, not from the request.
    let (status, body) = fixture.get(&format!("/playlist/overrides?scope=target&name={M3U_TARGET}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["entries"][0]["hint"], json!("Group / One"), "{body}");

    // The reverse direction holds too: an input save leaves the target file alone.
    let target_before = std::fs::read(&target_file).expect("target file");
    let input_base = fixture.revision("input", CONFIG_INPUT).await;
    let body = save_body(
        "input",
        CONFIG_INPUT,
        &input_base,
        json!([patch(None, "live", "p1", json!({ "group": set("Input Group") }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(fixture.input_file(CONFIG_INPUT).exists());
    assert_eq!(std::fs::read(&target_file).expect("target file"), target_before, "the target file is untouched");

    let stored = std::fs::read_to_string(fixture.input_file(CONFIG_INPUT)).expect("input file");
    assert!(stored.contains("Input Group"), "{stored}");
    assert!(!stored.contains(CONFIG_INPUT), "an input file carries no input name: {stored}");
}

#[tokio::test]
async fn provenance_reports_every_layer() {
    let fixture = fixture();
    // Two stored records share the override key of the xtream input (`provider_id` 42).
    fixture.write_xtream_input_playlist(
        OTHER_INPUT,
        vec![
            xtream_record(1, 42, "One", "Group", OTHER_INPUT),
            xtream_record(2, 42, "One Again", "Group", OTHER_INPUT),
        ],
    );
    fixture.write_target_playlist(
        M3U_TARGET,
        vec![m3u_record_of(OTHER_INPUT, "42", "Processed", "Group", PlaylistItemType::Live)],
    );

    let base = fixture.revision("input", OTHER_INPUT).await;
    let body =
        save_body("input", OTHER_INPUT, &base, json!([patch(None, "live", "42", json!({ "name": set("Edited") }))]));
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) =
        fixture.get(&format!("/playlist/overrides/item?target_id=1&input={OTHER_INPUT}&kind=live&id=42")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["name"], "One", "{body}");
    assert_eq!(body["provider"]["group"], "Group", "{body}");
    assert_eq!(body["input_override"]["name"], "Edited", "{body}");
    assert!(body["target_override"].is_null(), "{body}");
    assert_eq!(body["processed"]["name"], "Processed", "{body}");
    assert_eq!(body["ambiguous"], true, "the same key is stored twice in the input: {body}");
    let pending = body["pending_per_target"].as_array().expect("pending per target");
    assert_eq!(pending.len(), 1, "a target-scoped request reports that target: {body}");
    assert_eq!(pending[0], json!([M3U_TARGET, true]), "{body}");

    // Without a target the request reports every target the input feeds.
    let (status, body) = fixture.get(&format!("/playlist/overrides/item?input={OTHER_INPUT}&kind=live&id=42")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["processed"].is_null(), "no target, no processed item: {body}");
    assert!(body["target_override"].is_null(), "{body}");
    let pending = body["pending_per_target"].as_array().expect("pending per target");
    assert_eq!(pending.len(), 2, "both targets of the input are reported: {body}");
    assert!(pending.iter().all(|entry| entry[1] == true), "{body}");

    // A recorded successful run clears the pending state of that target.
    let input_scope = OverrideScope::Input(OTHER_INPUT.to_string());
    let revision = OverrideRevision(fixture.revision("input", OTHER_INPUT).await);
    fixture
        .app_state
        .override_status_store()
        .record_success(
            M3U_TARGET,
            vec![(input_scope, revision)],
            tuliprox_repository::OverrideRunDiagnostics::default(),
        )
        .await
        .expect("status recorded");
    let (status, body) = fixture.get(&format!("/playlist/overrides/item?input={OTHER_INPUT}&kind=live&id=42")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pending = body["pending_per_target"].as_array().expect("pending per target");
    let by_target = |name: &str| pending.iter().find(|entry| entry[0] == name).map(|entry| entry[1].clone());
    assert_eq!(by_target(M3U_TARGET), Some(json!(false)), "{body}");
    assert_eq!(by_target(XTREAM_TARGET), Some(json!(true)), "the other target has not run: {body}");

    // The discard removes the entry again and reports the new revision.
    let revision = fixture.revision("input", OTHER_INPUT).await;
    let discard = json!({
        "scope": "input",
        "name": OTHER_INPUT,
        "base_revision": revision,
        "items": [{ "kind": "live", "id": "42" }]
    });
    let (status, body) = fixture.post("/playlist/overrides/discard", discard).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revision"].as_str(), Some(OverrideRevision::empty().to_string().as_str()), "{body}");
    assert!(!fixture.input_file(OTHER_INPUT).exists(), "the emptied file is removed");
}

#[tokio::test]
async fn override_routes_are_mounted_without_authentication() {
    let fixture = fixture();

    // Every override endpoint answers on the auth-disabled registration; without the registration they
    // would all be 404, which is exactly what a local unauthenticated server would show.
    let (status, body) =
        fixture.call_unauthenticated("GET", "/playlist/overrides?scope=input&name=m3u-input", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let base = body["revision"].as_str().expect("revision").to_string();

    for uri in [
        "/playlist/overrides/item?input=m3u-input&kind=live&id=p1",
        "/playlist/overrides/orphans?input=m3u-input",
        "/playlist/epg/search?input=m3u-input&q=news",
        "/playlist/epg/search?target_id=1&q=news",
    ] {
        let (status, body) = fixture.call_unauthenticated("GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }

    let save =
        save_body("input", CONFIG_INPUT, &base, json!([patch(None, "live", "p1", json!({ "group": set("News") }))]));
    let (status, body) = fixture.call_unauthenticated("POST", "/playlist/overrides", Some(save)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let base = body["revision"].as_str().expect("revision").to_string();

    let discard = json!({
        "scope": "input",
        "name": CONFIG_INPUT,
        "base_revision": base,
        "items": [{ "kind": "live", "id": "p1" }]
    });
    let (status, body) = fixture.call_unauthenticated("POST", "/playlist/overrides/discard", Some(discard)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn epg_search_for_input_reads_only_cached_files() {
    let fixture = fixture();
    let epg_url = "http://provider.example/xmltv.php?username=secret-user&password=secret-pass";
    let epg_input = input_with_epg(1, CONFIG_INPUT, InputType::M3u, epg_url);
    fixture.install_sources(
        vec![Arc::clone(&epg_input), input(2, OTHER_INPUT, InputType::Xtream)],
        vec![ConfigSource {
            inputs: vec![CONFIG_INPUT.intern(), OTHER_INPUT.intern()],
            targets: vec![target(1, M3U_TARGET, m3u_output()), target(2, XTREAM_TARGET, xtream_output())],
        }],
    );

    let raw_epg_path = fixture.raw_epg_path(&epg_input).await;
    assert!(!raw_epg_path.exists(), "the fixture starts without a cache file");

    // No cache file: the source contributes nothing, no provider request is made (an attempt would either
    // fail and answer 500 or write a cache file) and the answer arrives without waiting on the network.
    let started = std::time::Instant::now();
    let (status, body) = fixture.get(&format!("/playlist/epg/search?input={CONFIG_INPUT}&q=news")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "the search waited: {:?}", started.elapsed());
    assert!(!raw_epg_path.exists(), "an absent cache file stays absent: {}", raw_epg_path.display());

    // A stored cache file is searched.
    std::fs::create_dir_all(raw_epg_path.parent().expect("parent")).expect("epg dir");
    std::fs::write(&raw_epg_path, xmltv_cache(&[("news-a", "News A"), ("sports", "Sports")])).expect("epg cache");
    let (status, body) = fixture.get(&format!("/playlist/epg/search?input={CONFIG_INPUT}&q=news")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body.as_array().expect("array").iter().filter_map(|entry| entry["id"].as_str()).collect();
    assert_eq!(ids, vec!["news-a"], "only the stored channel matches: {body}");

    // An unreadable cache file answers with no candidates instead of failing.
    std::fs::write(&raw_epg_path, b"<<<not xml").expect("corrupt epg cache");
    let (status, body) = fixture.get(&format!("/playlist/epg/search?input={CONFIG_INPUT}&q=news")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");
}

#[tokio::test]
async fn epg_search_target_without_database_is_empty() {
    let fixture = fixture();

    // A configured target that has not run an update yet has no EPG database: that is an empty result,
    // not a server error.
    let (status, body) = fixture.get("/playlist/epg/search?target_id=1&q=news").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]), "{body}");

    // An unknown target id stays a 404.
    let (status, _) = fixture.get("/playlist/epg/search?target_id=9999&q=news").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn orphaned_file_can_be_discarded() {
    let fixture = fixture();
    let path = fixture.input_file("old-input");
    write_override_file(&path, "schema_version: 1\nlive:\n  p1:\n    group: News\n");

    let (status, body) = fixture.get(&format!("/playlist/overrides/orphans?input={CONFIG_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let revision = body["orphaned_files"]
        .as_array()
        .expect("orphaned files")
        .iter()
        .find(|file| file["name"] == "old-input")
        .and_then(|file| file["revision"].as_str())
        .expect("orphan revision")
        .to_string();

    // A stale base revision never deletes the file.
    let stale = json!({ "scope": "input", "name": "old-input", "base_revision": "stale", "items": [] });
    let (status, body) = fixture.post("/playlist/overrides/discard", stale).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["current_revision"].as_str(), Some(revision.as_str()), "{body}");
    assert!(path.exists(), "a rejected discard keeps the file");

    // A matching base revision deletes the orphaned file as a whole.
    let discard = json!({ "scope": "input", "name": "old-input", "base_revision": revision, "items": [] });
    let (status, body) = fixture.post("/playlist/overrides/discard", discard).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revision"].as_str(), Some(OverrideRevision::empty().to_string().as_str()), "{body}");
    assert!(!path.exists(), "the whole file is gone");

    // Without a file on disk and without a configured name nothing is disclosed.
    let unknown = json!({ "scope": "input", "name": "no-such-input", "base_revision": "x", "items": [] });
    let (status, _) = fixture.post("/playlist/overrides/discard", unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn provenance_scan_is_scoped_to_the_item_kind() {
    let fixture = fixture();
    fixture.write_xtream_input_playlist(OTHER_INPUT, vec![xtream_record(1, 42, "Live One", "Group", OTHER_INPUT)]);

    // The live record is found when live is asked for.
    let (status, body) = fixture.get(&format!("/playlist/overrides/item?input={OTHER_INPUT}&kind=live&id=42")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["name"], "Live One", "{body}");

    // Asking for another kind never reports the live record: only the cluster of that kind is scanned.
    let (status, body) = fixture.get(&format!("/playlist/overrides/item?input={OTHER_INPUT}&kind=vod&id=42")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["provider"].is_null(), "{body}");
    assert_eq!(body["ambiguous"], false, "{body}");
}

#[tokio::test]
async fn provenance_reports_alias_targets_once() {
    let fixture = fixture();
    fixture.install_sources(
        vec![alias_input(1, CONFIG_INPUT, "m3u-alias")],
        vec![
            ConfigSource { inputs: vec![CONFIG_INPUT.intern()], targets: vec![target(1, M3U_TARGET, m3u_output())] },
            ConfigSource {
                inputs: vec!["m3u-alias".intern()],
                targets: vec![target(1, M3U_TARGET, m3u_output()), target(2, XTREAM_TARGET, xtream_output())],
            },
        ],
    );

    let (status, body) = fixture.get(&format!("/playlist/overrides/item?input={CONFIG_INPUT}&kind=live&id=p1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pending = body["pending_per_target"].as_array().expect("pending per target");
    let mut names: Vec<&str> = pending.iter().filter_map(|entry| entry[0].as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec![M3U_TARGET, XTREAM_TARGET], "a source naming the alias is a consumer too: {body}");
    assert_eq!(names.len(), 2, "a target referenced twice is reported once: {body}");
}

/// The same record shape in a movie cluster, so one input can hold two kinds with one key.
fn vod_record(virtual_id: u32, provider_id: u32, name: &str, group: &str, input_name: &str) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        xtream_cluster: XtreamCluster::Video,
        item_type: PlaylistItemType::Video,
        category_id: 2,
        ..xtream_record(virtual_id, provider_id, name, group, input_name)
    }
}

#[tokio::test]
async fn hints_are_read_from_the_matching_cluster() {
    let fixture = fixture();
    fixture.write_xtream_input_cluster(
        OTHER_INPUT,
        XtreamCluster::Live,
        vec![xtream_record(1, 42, "Live One", "Live Group", OTHER_INPUT)],
    );
    fixture.write_xtream_input_cluster(
        OTHER_INPUT,
        XtreamCluster::Video,
        vec![vod_record(2, 42, "Movie One", "Movie Group", OTHER_INPUT)],
    );

    let base = fixture.revision("input", OTHER_INPUT).await;
    let body = save_body(
        "input",
        OTHER_INPUT,
        &base,
        json!([patch(None, "vod", "42", json!({ "name": set("Renamed Movie") }))]),
    );
    let (status, body) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = fixture.get(&format!("/playlist/overrides?scope=input&name={OTHER_INPUT}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["entries"][0]["hint"],
        json!("Movie Group / Movie One"),
        "the hint comes from the movie cluster, not from the live one: {body}"
    );
}

#[tokio::test]
async fn test_b1_aliased_input_resolves_to_parent() {
    let fixture = fixture();
    let parent = alias_input(10, "parent-input", "alias-input");
    let sources = vec![ConfigSource {
        inputs: vec!["parent-input".intern()],
        targets: vec![target(1, M3U_TARGET, m3u_output())],
    }];
    fixture.install_sources(vec![parent], sources);

    // Save an override on parent-input
    let base = fixture.revision("input", "parent-input").await;
    let body =
        save_body("input", "parent-input", &base, json!([patch(None, "live", "p1", json!({ "name": set("Item 1") }))]));
    let (status, _) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK);

    // 1. GET /playlist/overrides?scope=input&name=alias-input resolves to parent-input
    let (status, body) = fixture.get("/playlist/overrides?scope=input&name=alias-input").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status_parent, body_parent) = fixture.get("/playlist/overrides?scope=input&name=parent-input").await;
    assert_eq!(status_parent, StatusCode::OK);
    assert_eq!(body["revision"], body_parent["revision"]);
    assert_eq!(body["entries"][0]["id"], "p1");

    // 2. Provenance with input=alias-input resolves to parent-input
    let (status, body) = fixture.get("/playlist/overrides/item?input=alias-input&kind=live&id=p1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["input_override"]["name"], "Item 1");

    // 3. Orphans with input=alias-input
    let (status, body) = fixture.get("/playlist/overrides/orphans?input=alias-input").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["entries"][0]["id"], "p1");

    // 4. EPG search with input=alias-input
    let (status, body) = fixture.get("/playlist/epg/search?input=alias-input&q=news").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn test_b2_orphan_detection_scans_aliases_and_missing_sets_unknown() {
    let fixture = fixture();
    let parent = alias_input(10, "parent-input", "alias-input");
    let sources = vec![ConfigSource {
        inputs: vec!["parent-input".intern()],
        targets: vec![target(1, M3U_TARGET, m3u_output())],
    }];
    fixture.install_sources(vec![parent], sources);

    // Save overrides on parent-input for p1 (present in parent) and p2 (present in alias)
    let base = fixture.revision("input", "parent-input").await;
    let body = save_body(
        "input",
        "parent-input",
        &base,
        json!([
            patch(None, "live", "p1", json!({ "name": set("Item 1") })),
            patch(None, "live", "p2", json!({ "name": set("Item 2") })),
            patch(None, "live", "p3", json!({ "name": set("Item 3") })),
        ]),
    );
    let (status, _) = fixture.post("/playlist/overrides", body).await;
    assert_eq!(status, StatusCode::OK);

    // Case A: alias storage missing or failed -> state must be unknown, NEVER orphaned
    fixture.write_input_playlist(
        "parent-input",
        vec![m3u_record_of("parent-input", "p1", "One", "G", PlaylistItemType::Live)],
    );
    fixture.write_input_status("parent-input", crate::processing::input_cache::ClusterState::Ok);
    // Note: alias-input status/storage NOT created!
    let (status, body) = fixture.get("/playlist/overrides/orphans?input=parent-input").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["entries"].as_array().expect("entries");
    for entry in entries {
        assert_eq!(entry["state"], "unknown", "missing alias inventory must make all entries unknown: {entry}");
    }

    // Case B: parent has p1, alias has p2, both loaded OK -> p1 and p2 active, p3 orphaned
    fixture.write_input_playlist(
        "alias-input",
        vec![m3u_record_of("alias-input", "p2", "Two", "G", PlaylistItemType::Live)],
    );
    fixture.write_input_status("alias-input", crate::processing::input_cache::ClusterState::Ok);

    let (status, body) = fixture.get("/playlist/overrides/orphans?input=parent-input").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let entries = body["entries"].as_array().expect("entries");
    let state_of = |id: &str| {
        entries
            .iter()
            .find(|entry| entry["id"] == id)
            .and_then(|entry| entry["state"].as_str())
            .unwrap_or("missing")
            .to_string()
    };
    assert_eq!(state_of("p1"), "active", "item in parent inventory is active");
    assert_eq!(state_of("p2"), "active", "item in alias inventory is active");
    assert_eq!(state_of("p3"), "orphaned", "item in neither is orphaned");
}

#[tokio::test]
async fn test_b4_explorer_flag_controls_meta_and_streaming() {
    use shared::model::UiPlaylistItem;

    let fixture = fixture();
    fixture.write_input_playlist(CONFIG_INPUT, vec![m3u_record("p1", "Item One", "Group One", PlaylistItemType::Live)]);

    // 1. Without explorer query param (default explorer=false)
    let (status, body) =
        fixture.call("POST", "/playlist/live", Some(&fixture.reader), Some(json!({ "Input": CONFIG_INPUT }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array of items");
    assert_eq!(items.len(), 1);
    let item: UiPlaylistItem = serde_json::from_value(items[0].clone()).expect("deserialize UiPlaylistItem");
    assert!(item.explorer_meta.is_none(), "explorer=false must not emit explorer_meta");

    // 2. With explorer=false explicitly
    let (status, body) = fixture
        .call("POST", "/playlist/live?explorer=false", Some(&fixture.reader), Some(json!({ "Input": CONFIG_INPUT })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array of items");
    assert_eq!(items.len(), 1);
    let item: UiPlaylistItem = serde_json::from_value(items[0].clone()).expect("deserialize UiPlaylistItem");
    assert!(item.explorer_meta.is_none(), "explorer=false must not emit explorer_meta");

    // 3. With explorer=true
    let (status, body) = fixture
        .call("POST", "/playlist/live?explorer=true", Some(&fixture.reader), Some(json!({ "Input": CONFIG_INPUT })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array of items");
    assert_eq!(items.len(), 1);
    let item: UiPlaylistItem = serde_json::from_value(items[0].clone()).expect("deserialize UiPlaylistItem");
    let meta = item.explorer_meta.expect("explorer=true must attach explorer_meta");
    assert_eq!(meta.override_id.as_deref(), Some("p1"));
    assert!(!meta.override_state.edited);
}

#[tokio::test]
async fn override_provenance_finds_xtream_target_records_from_m3u_input_without_numeric_id() {
    let fixture = fixture();
    let url1 = "http://192.168.20.2:5004/auto/v5.1";
    let url2 = "http://192.168.20.2:5004/auto/v5.2";
    let hash1 = extract_id_from_url(url1);
    let hash2 = extract_id_from_url(url2);

    let mut m3u1 = m3u_record_of(CONFIG_INPUT, &hash1, "WRAL HD", "Group", PlaylistItemType::Live);
    m3u1.url = url1.intern();
    m3u1.input_stream_id = hash1.clone().intern();

    let mut m3u2 = m3u_record_of(CONFIG_INPUT, &hash2, "WRAL DT", "Group", PlaylistItemType::Live);
    m3u2.url = url2.intern();
    m3u2.input_stream_id = hash2.clone().intern();

    fixture.write_input_playlist(CONFIG_INPUT, vec![m3u1, m3u2]);

    let mut xtream1 = xtream_record(1, 0, "WRAL HD Processed", "Group", CONFIG_INPUT);
    xtream1.url = url1.intern();
    xtream1.input_stream_id = hash1.clone().intern();

    let mut xtream2 = xtream_record(2, 0, "WRAL DT Processed", "Group", CONFIG_INPUT);
    xtream2.url = url2.intern();
    xtream2.input_stream_id = hash2.clone().intern();

    fixture.write_xtream_target_playlist(XTREAM_TARGET, vec![xtream1, xtream2]);

    let sources = fixture.app_state.app_config.sources.load();
    let aliases = tuliprox_core::model::InputAliasMap::from_inputs(&sources.inputs);
    let key1 = tuliprox_core::model::override_key(
        &shared::model::PlaylistItemHeader {
            id: hash1.clone().intern(),
            url: url1.intern(),
            input_name: CONFIG_INPUT.intern(),
            item_type: PlaylistItemType::Live,
            ..Default::default()
        },
        &aliases,
    )
    .expect("override key 1")
    .id;
    let key2 = tuliprox_core::model::override_key(
        &shared::model::PlaylistItemHeader {
            id: hash2.clone().intern(),
            url: url2.intern(),
            input_name: CONFIG_INPUT.intern(),
            item_type: PlaylistItemType::Live,
            ..Default::default()
        },
        &aliases,
    )
    .expect("override key 2")
    .id;

    // Query for 5.1:
    let (status, body) =
        fixture.get(&format!("/playlist/overrides/item?target_id=2&input={CONFIG_INPUT}&kind=live&id={key1}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["name"], "WRAL HD", "{body}");
    assert_eq!(body["processed"]["name"], "WRAL HD Processed", "{body}");

    // Query for 5.2:
    let (status, body) =
        fixture.get(&format!("/playlist/overrides/item?target_id=2&input={CONFIG_INPUT}&kind=live&id={key2}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["name"], "WRAL DT", "{body}");
    assert_eq!(body["processed"]["name"], "WRAL DT Processed", "{body}");
}

#[tokio::test]
async fn input_explorer_for_m3u_input_returns_distinct_nonzero_virtual_ids() {
    let fixture = fixture();
    let mut item1 = m3u_record_of(CONFIG_INPUT, "p1", "Item One", "Group", PlaylistItemType::Live);
    item1.virtual_id = VirtualId::default();
    let mut item2 = m3u_record_of(CONFIG_INPUT, "p2", "Item Two", "Group", PlaylistItemType::Live);
    item2.virtual_id = VirtualId::default();

    fixture.write_input_playlist(CONFIG_INPUT, vec![item1, item2]);

    let (status, body) = fixture
        .call("POST", "/playlist/live?explorer=true", Some(&fixture.reader), Some(json!({ "Input": CONFIG_INPUT })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array of items");
    assert_eq!(items.len(), 2, "{body}");
    let item1: UiPlaylistItem = serde_json::from_value(items[0].clone()).expect("item1");
    let item2: UiPlaylistItem = serde_json::from_value(items[1].clone()).expect("item2");

    assert_ne!(item1.virtual_id, 0, "item1 virtual_id must be non-zero in explorer mode");
    assert_ne!(item2.virtual_id, 0, "item2 virtual_id must be non-zero in explorer mode");
    assert_ne!(item1.virtual_id, item2.virtual_id, "virtual_ids must be distinct");

    // Non-explorer mode leaves raw input records with virtual_id == 0:
    let (status, body) = fixture
        .call("POST", "/playlist/live?explorer=false", Some(&fixture.reader), Some(json!({ "Input": CONFIG_INPUT })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("array of items");
    let non_exp1: UiPlaylistItem = serde_json::from_value(items[0].clone()).expect("non_exp1");
    let non_exp2: UiPlaylistItem = serde_json::from_value(items[1].clone()).expect("non_exp2");
    assert_eq!(non_exp1.virtual_id, 0);
    assert_eq!(non_exp2.virtual_id, 0);
}

#[tokio::test]
async fn scan_target_item_with_mapped_url_returns_processed_fields_for_input_key() {
    let fixture = fixture();
    let url = "http://host:5004/auto/v5.2";
    let hash = extract_id_from_url(url);

    let mut m3u_input = m3u_record_of(CONFIG_INPUT, &hash, "WRAL DT", "Group", PlaylistItemType::Live);
    m3u_input.url = url.intern();
    m3u_input.input_stream_id = hash.clone().intern();
    fixture.write_input_playlist(CONFIG_INPUT, vec![m3u_input]);

    let mapped_url = "http://other/live/123.ts";

    // 1. Xtream target with mapped URL
    let mut xtream_target_item = xtream_record(1, 0, "WRAL DT Processed Xtream", "Group", CONFIG_INPUT);
    xtream_target_item.url = mapped_url.intern();
    xtream_target_item.input_url = url.intern();
    xtream_target_item.input_stream_id = hash.clone().intern();
    fixture.write_xtream_target_playlist(XTREAM_TARGET, vec![xtream_target_item]);

    // 2. M3U target with rewritten provider_id and mapped URL
    let mut m3u_target_item =
        m3u_record_of(CONFIG_INPUT, "mapped-provider-id", "WRAL DT Processed M3U", "Group", PlaylistItemType::Live);
    m3u_target_item.url = mapped_url.intern();
    m3u_target_item.input_url = url.intern();
    m3u_target_item.input_stream_id = hash.clone().intern();
    fixture.write_target_playlist(M3U_TARGET, vec![m3u_target_item]);

    let sources = fixture.app_state.app_config.sources.load();
    let aliases = tuliprox_core::model::InputAliasMap::from_inputs(&sources.inputs);
    let key = tuliprox_core::model::override_key(
        &shared::model::PlaylistItemHeader {
            id: hash.intern(),
            url: url.intern(),
            input_name: CONFIG_INPUT.intern(),
            item_type: PlaylistItemType::Live,
            ..Default::default()
        },
        &aliases,
    )
    .expect("override key")
    .id;

    // Query target 2 (Xtream) using the input override key:
    let (status, body) =
        fixture.get(&format!("/playlist/overrides/item?target_id=2&input={CONFIG_INPUT}&kind=live&id={key}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["name"], "WRAL DT", "{body}");
    assert_eq!(body["processed"]["name"], "WRAL DT Processed Xtream", "{body}");

    // Query target 1 (M3U) using the input override key:
    let (status, body) =
        fixture.get(&format!("/playlist/overrides/item?target_id=1&input={CONFIG_INPUT}&kind=live&id={key}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["name"], "WRAL DT", "{body}");
    assert_eq!(body["processed"]["name"], "WRAL DT Processed M3U", "{body}");
}
