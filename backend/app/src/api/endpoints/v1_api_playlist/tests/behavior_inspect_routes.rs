use crate::{
    api::{
        endpoints::v1_api_playlist::{
            items::tests::{m3u_item, xtream_item, SECRET_URL},
            v1_api_playlist_register_with_permissions,
        },
        model::{create_test_app_state, AppState},
    },
    auth::create_jwt_web_user,
    model::{
        Config, ConfigInput, ConfigSource, ConfigTarget, M3uTargetOutput, SourcesConfig, TargetOutput,
        XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    repository::{
        get_target_storage_path, m3u_get_epg_file_path_for_target, m3u_get_file_path_for_db,
        xtream_get_epg_file_path_for_target, xtream_get_file_path, xtream_get_storage_path, BPlusTree,
    },
};
use arc_swap::ArcSwapOption;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use chrono::Utc;
use serde_json::Value;
use shared::{
    foundation::Filter,
    model::{
        ConfigTargetOptions, EpgChannel, EpgOutputOptions, EpgProgramme, InputType, M3uPlaylistItem, Permission,
        PlaylistItemType, ProcessingOrder, UserId, WebAuthConfigDto, WebUiConfigDto, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    app_state: Arc<AppState>,
    router: Router,
    reader: String,
    epg_only: String,
}

fn target(id: u16, name: &str, output: TargetOutput, lowercase_ids: bool) -> Arc<ConfigTarget> {
    Arc::new(ConfigTarget {
        curation: None,
        id,
        enabled: true,
        name: name.to_string(),
        options: lowercase_ids.then(|| ConfigTargetOptions {
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

fn xtream_output() -> TargetOutput {
    TargetOutput::Xtream(XtreamTargetOutput { flags: XtreamTargetFlagsSet::new(), trakt: None, filter: None })
}

fn m3u_output() -> TargetOutput {
    TargetOutput::M3u(M3uTargetOutput {
        filename: None,
        include_type_in_url: false,
        mask_redirect_url: false,
        filter: None,
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

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let web_ui = WebUiConfigDto {
        auth: Some(WebAuthConfigDto {
            enabled: true,
            issuer: "inspect-test".to_owned(),
            secret: "inspect-test-secret".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config = Config {
        web_ui: Some((&web_ui).into()),
        storage_dir: dir.path().to_string_lossy().into_owned(),
        ..Config::default()
    };
    let web_auth = config.web_ui.as_ref().and_then(|ui| ui.auth.as_ref()).expect("web auth");
    let token = |name: &str, permission: Permission| {
        create_jwt_web_user(web_auth, name, permission.into(), 0, UserId::from(format!("web:{name}").as_str()))
            .expect("jwt")
    };
    let reader = token("reader", Permission::PlaylistRead);
    let epg_only = token("epg_only", Permission::EpgRead);

    let app_state = create_test_app_state(config);
    let inputs = vec![
        input(1, "m3u-input", InputType::M3u),
        input(2, "xtream-input", InputType::Xtream),
        input(3, "stalker-input", InputType::Stalker),
    ];
    let sources = SourcesConfig {
        group_lookup: shared::model::provider_saturation::build_group_lookup(&inputs),
        inputs,
        sources: vec![ConfigSource {
            inputs: vec!["m3u-input".intern(), "xtream-input".intern(), "stalker-input".intern()],
            targets: vec![
                target(1, "m3u-target", m3u_output(), false),
                target(2, "xtream-target", xtream_output(), false),
                target(3, "folded-target", xtream_output(), true),
            ],
        }],
        ..SourcesConfig::default()
    };
    app_state.app_config.sources.store(Arc::new(sources));
    let router =
        v1_api_playlist_register_with_permissions(Router::new(), &app_state).with_state(Arc::clone(&app_state));
    Fixture { _dir: dir, app_state, router, reader, epg_only }
}

impl Fixture {
    fn config(&self) -> Arc<Config> { self.app_state.app_config.config.load_full() }

    async fn call(&self, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = Request::builder().method("GET").uri(uri);
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response =
            self.router.clone().oneshot(request.body(Body::empty()).expect("request")).await.expect("response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await.expect("body");
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) { self.call(uri, Some(&self.reader.clone())).await }

    fn write_m3u_target(&self, target: &str, items: Vec<M3uPlaylistItem>) {
        let path = m3u_get_file_path_for_db(&get_target_storage_path(&self.config(), target).expect("target path"));
        let mut tree = BPlusTree::<u32, M3uPlaylistItem>::new();
        for item in items {
            tree.insert(item.virtual_id.get(), item);
        }
        store(&path, tree);
    }

    fn write_xtream_target(&self, target: &str, cluster: XtreamCluster, items: Vec<XtreamPlaylistItem>) {
        let storage = xtream_get_storage_path(&self.config(), target).expect("xtream path");
        let mut tree = BPlusTree::<u32, XtreamPlaylistItem>::new();
        for item in items {
            tree.insert(item.virtual_id.get(), item);
        }
        store(&xtream_get_file_path(&storage, cluster), tree);
    }

    fn write_m3u_epg(&self, target: &str, channels: &[EpgChannel]) {
        write_epg(
            &m3u_get_epg_file_path_for_target(&get_target_storage_path(&self.config(), target).unwrap()),
            channels,
        );
    }

    fn write_xtream_epg(&self, target: &str, channels: &[EpgChannel]) {
        write_epg(
            &xtream_get_epg_file_path_for_target(&xtream_get_storage_path(&self.config(), target).unwrap()),
            channels,
        );
    }
}

fn store<K, V>(path: &Path, mut tree: BPlusTree<K, V>)
where
    K: Ord + Clone + serde::Serialize + for<'de> serde::Deserialize<'de>,
    V: Clone + serde::Serialize + for<'de> serde::Deserialize<'de>,
{
    std::fs::create_dir_all(path.parent().expect("parent")).expect("storage dir");
    tree.store(path).expect("db written");
}

fn write_epg(path: &Path, channels: &[EpgChannel]) {
    let mut tree = BPlusTree::<Arc<str>, EpgChannel>::new();
    for channel in channels {
        tree.insert(Arc::clone(&channel.id), channel.clone());
    }
    store(path, tree);
}

fn epg(id: &str, title: Option<&str>, programmes: Vec<(i64, i64)>) -> EpgChannel {
    EpgChannel {
        id: id.intern(),
        title: title.map(Internable::intern),
        icon: None,
        programmes: programmes.into_iter().map(|(start, stop)| EpgProgramme::new(start, stop, id.intern())).collect(),
    }
}

/// Channels exercising every issue; `current` and `orphan` are healthy otherwise.
fn issue_channels() -> Vec<EpgChannel> {
    let now = Utc::now().timestamp();
    vec![
        epg("current", Some("Current Channel"), vec![(now - 600, now + 600)]),
        epg("empty", Some("Empty Channel"), vec![]),
        epg("expired", Some("Expired Channel"), vec![(now - 7200, now - 3600)]),
        epg("future", Some("Future Channel"), vec![(now + 3600, now + 7200)]),
        epg("orphan", Some("Orphan Channel"), vec![(now - 600, now + 600)]),
        epg("untitled", None, vec![(now - 600, now + 600)]),
    ]
}

fn live_items_for_issue_channels() -> Vec<M3uPlaylistItem> {
    (1_u32..)
        .zip(["current", "empty", "expired", "future", "untitled"])
        .map(|(virtual_id, id)| m3u_item(virtual_id, &format!("p{virtual_id}"), id, "Live", Some(id)))
        .collect()
}

fn ids(value: &Value, key: &str) -> Vec<String> {
    value[key]
        .as_array()
        .expect("array")
        .iter()
        .map(|entry| entry["id"].as_str().unwrap_or_else(|| entry["name"].as_str().unwrap_or_default()).to_string())
        .collect()
}

#[tokio::test]
async fn read_routes_require_playlist_read() {
    let fixture = fixture();
    for uri in [
        "/playlist/sources",
        "/playlist/items?target=m3u-target&cluster=live",
        "/playlist/epg/channels?target=m3u-target",
        "/playlist/epg/channel?target=m3u-target&channel_id=x",
    ] {
        let (status, _) = fixture.call(uri, Some(&fixture.epg_only)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        let (status, _) = fixture.call(uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        let (status, _) = fixture.call(uri, Some(&fixture.reader)).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{uri}");
        assert_ne!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
}

#[tokio::test]
async fn sources_lists_targets_and_inputs_without_credentials() {
    let fixture = fixture();
    fixture.write_m3u_epg("m3u-target", &[epg("a", None, vec![])]);
    let (status, body) = fixture.get("/playlist/sources").await;
    assert_eq!(status, StatusCode::OK);

    let targets = body["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 3);
    let m3u = targets.iter().find(|t| t["name"] == "m3u-target").unwrap();
    assert_eq!(m3u["id"], 1);
    assert_eq!(m3u["outputs"], serde_json::json!(["m3u"]));
    assert_eq!(m3u["epg"], serde_json::json!([{ "output": "m3u", "available": true }]));
    let xtream = targets.iter().find(|t| t["name"] == "xtream-target").unwrap();
    assert_eq!(xtream["epg"], serde_json::json!([{ "output": "xtream", "available": false }]));

    let inputs = body["inputs"].as_array().unwrap();
    let by_name = |name: &str| inputs.iter().find(|i| i["name"] == name).unwrap();
    assert_eq!(by_name("m3u-input")["input_type"], "m3u");
    assert_eq!(by_name("m3u-input")["clusters"], serde_json::json!(["live", "video", "series"]));
    assert_eq!(by_name("xtream-input")["clusters"], serde_json::json!(["live", "video", "series"]));
    assert_eq!(by_name("stalker-input")["input_type"], "stalker");
    assert_eq!(by_name("stalker-input")["clusters"], serde_json::json!([]));

    let text = body.to_string();
    for secret in ["secret-user", "secret-pass", "provider.example", "http"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

#[tokio::test]
async fn items_rejects_bad_parameters_with_400() {
    let fixture = fixture();
    for (uri, expected) in [
        ("/playlist/items?cluster=live", "Exactly one"),
        ("/playlist/items?target=m3u-target&input=m3u-input&cluster=live", "not both"),
        ("/playlist/items?target=m3u-target", "Missing cluster"),
        ("/playlist/items?target=m3u-target&cluster=vod", "allowed values: live, video, series"),
        ("/playlist/items?target=m3u-target&cluster=live&fields=url", "Unknown fields 'url'"),
        ("/playlist/items?target=m3u-target&cluster=live&search_field=nope", "Unknown search_field"),
        ("/playlist/items?target=m3u-target&cluster=live&epg_status=nope", "missing_id, unmatched, matched"),
        ("/playlist/items?input=m3u-input&cluster=live&epg_status=matched", "only available for a target"),
        ("/playlist/items?input=m3u-input&cluster=live&fields=epg_status", "only available for a target"),
    ] {
        let (status, body) = fixture.get(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(body["error"].as_str().unwrap_or_default().contains(expected), "{uri}: {body}");
    }
    let (status, _) = fixture.get("/playlist/items?target=m3u-target&cluster=live&limit=abc").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn items_unknown_source_is_404_and_stalker_input_is_400() {
    let fixture = fixture();
    let (status, body) = fixture.get("/playlist/items?target=missing&cluster=live").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("Unknown target 'missing'"));
    let (status, body) = fixture.get("/playlist/items?input=missing&cluster=live").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("Unknown input 'missing'"));

    let (status, body) = fixture.get("/playlist/items?input=stalker-input&cluster=live").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("only xtream and m3u inputs"), "{body}");
}

#[tokio::test]
async fn items_of_a_source_without_a_stored_database_are_empty_with_a_note() {
    let fixture = fixture();
    for uri in [
        "/playlist/items?target=m3u-target&cluster=live",
        "/playlist/items?target=xtream-target&cluster=series",
        "/playlist/items?input=m3u-input&cluster=live",
        "/playlist/items?input=xtream-input&cluster=video",
    ] {
        let (status, body) = fixture.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["total_matched"], 0, "{uri}");
        assert_eq!(body["returned"], 0);
        assert_eq!(body["has_more"], false);
        assert_eq!(body["items"], serde_json::json!([]));
        assert!(body["note"].as_str().unwrap().contains("never updated"), "{uri}: {body}");
    }
}

#[tokio::test]
async fn items_pages_filters_and_projects_a_stored_m3u_target() {
    let fixture = fixture();
    fixture.write_m3u_target(
        "m3u-target",
        vec![
            m3u_item(1, "p1", "BBC One", "UK", Some("bbc.one")),
            m3u_item(2, "p2", "BBC Two", "UK", Some("bbc.two")),
            m3u_item(3, "p3", "CNN News", "US", None),
            m3u_item(4, "p4", "Sky Sports", "UK", Some("sky.sports")),
        ],
    );

    let (status, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], serde_json::json!({ "kind": "target", "name": "m3u-target" }));
    assert_eq!(body["cluster"], "live");
    assert_eq!(
        (body["total_matched"].clone(), body["returned"].clone(), body["has_more"].clone()),
        (4.into(), 2.into(), true.into())
    );
    assert_eq!(
        (body["offset"].clone(), body["limit"].clone(), body["limit_clamped"].clone()),
        (0.into(), 2.into(), false.into())
    );
    assert_eq!(ids(&body, "items"), vec!["BBC One", "BBC Two"]);
    let first = &body["items"][0];
    assert_eq!(first["virtual_id"], 1);
    assert_eq!(first["provider_id"], "p1");
    assert_eq!(first["epg_channel_id"], "bbc.one");
    assert_eq!(first["cluster"], "live");
    assert!(first.get("logo").is_none(), "opt-in fields are not in the default projection");

    // Last page, offset window.
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&offset=3&limit=10").await;
    assert_eq!(ids(&body, "items"), vec!["Sky Sports"]);
    assert_eq!(body["has_more"], false);
    // Offset beyond the total.
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&offset=99").await;
    assert_eq!(
        (body["total_matched"].clone(), body["returned"].clone(), body["has_more"].clone()),
        (4.into(), 0.into(), false.into())
    );
    // limit=0 means default, a huge limit is clamped and reported.
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&limit=0").await;
    assert_eq!((body["limit"].clone(), body["limit_clamped"].clone()), (50.into(), false.into()));
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&limit=9999").await;
    assert_eq!((body["limit"].clone(), body["limit_clamped"].clone()), (500.into(), true.into()));

    // Search (case-insensitive, default fields) combined with an exact group.
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&search=BBC&group=UK").await;
    assert_eq!(body["total_matched"], 2);
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&search=news&group=UK").await;
    assert_eq!(body["total_matched"], 0);
    // search_field restricts the search, repeatable.
    let (_, body) = fixture
        .get("/playlist/items?target=m3u-target&cluster=live&search=sky&search_field=epg_channel_id&search_field=input_name")
        .await;
    assert_eq!(ids(&body, "items"), vec!["Sky Sports"]);
    // Projection: requested fields only, in order.
    let (_, body) = fixture
        .get("/playlist/items?target=m3u-target&cluster=live&limit=1&fields=name&fields=logo&fields=source_ordinal")
        .await;
    let keys: Vec<&str> = body["items"][0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["name", "logo", "source_ordinal"]);
    // Other clusters of the m3u DB are filtered by item type.
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=video").await;
    assert_eq!(body["total_matched"], 0);
    assert!(body.get("note").is_none(), "the database exists");
}

#[tokio::test]
async fn items_never_leak_the_provider_url_or_credentials() {
    let fixture = fixture();
    let mut item = m3u_item(1, "p1", "Chan", "G", Some("e"));
    item.additional_properties =
        Some(shared::model::StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
            direct_source: SECRET_URL.intern(),
            ..shared::model::LiveStreamProperties::default()
        })));
    fixture.write_m3u_target("m3u-target", vec![item]);
    let fields = crate::api::endpoints::v1_api_playlist::items::ItemField::NAMES
        .iter()
        .filter(|name| **name != "epg_status")
        .map(|name| format!("fields={name}"))
        .collect::<Vec<_>>()
        .join("&");
    let (status, body) = fixture.get(&format!("/playlist/items?target=m3u-target&cluster=live&{fields}")).await;
    assert_eq!(status, StatusCode::OK);
    let text = body.to_string();
    for secret in ["secret-user", "secret-pass", "provider.example"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    assert!(body["items"][0].get("url").is_none());
    assert!(body["items"][0]["additional_properties"]["Live"].get("direct_source").is_none());
}

#[tokio::test]
async fn items_series_cluster_of_an_xtream_target_hides_episode_rows() {
    let fixture = fixture();
    let series = |virtual_id: u32, name: &str, item_type: PlaylistItemType| {
        let mut item = xtream_item(virtual_id, virtual_id, name, "Shows", None);
        item.item_type = item_type;
        item.xtream_cluster = XtreamCluster::Series;
        item
    };
    fixture.write_xtream_target(
        "xtream-target",
        XtreamCluster::Series,
        vec![
            series(1, "Show", PlaylistItemType::SeriesInfo),
            series(2, "Episode", PlaylistItemType::Series),
            series(3, "Other Show", PlaylistItemType::SeriesInfo),
        ],
    );
    let (status, body) = fixture.get("/playlist/items?target=xtream-target&cluster=series").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&body, "items"), vec!["Show", "Other Show"]);
    assert_eq!(body["total_matched"], 2);
    assert_eq!(body["items"][0]["category_id"], 9);

    // An input listing is not filtered, like `get_playlist_for_input`.
    let storage =
        crate::repository::get_input_storage_path("xtream-input", &fixture.config().storage_dir).await.unwrap();
    let mut tree = BPlusTree::<u32, XtreamPlaylistItem>::new();
    for (id, item_type) in [(1, PlaylistItemType::SeriesInfo), (2, PlaylistItemType::Series)] {
        let mut item = xtream_item(id, id, "x", "Shows", None);
        item.item_type = item_type;
        item.xtream_cluster = XtreamCluster::Series;
        tree.insert(id, item);
    }
    store(&xtream_get_file_path(&storage, XtreamCluster::Series), tree);
    let (_, body) = fixture.get("/playlist/items?input=xtream-input&cluster=series").await;
    assert_eq!(body["total_matched"], 2);
    assert_eq!(body["source"], serde_json::json!({ "kind": "input", "name": "xtream-input" }));
}

#[tokio::test]
async fn items_epg_status_filters_against_the_target_epg_database() {
    let fixture = fixture();
    fixture.write_m3u_target(
        "m3u-target",
        vec![
            m3u_item(1, "p1", "Matched", "g", Some("known")),
            m3u_item(2, "p2", "Unmatched", "g", Some("unknown")),
            m3u_item(3, "p3", "Missing", "g", None),
            m3u_item(4, "p4", "Blank", "g", Some("  ")),
        ],
    );

    // No EPG database yet: the parameter cannot be evaluated.
    let (status, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&epg_status=matched").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("no EPG database"), "{body}");
    let (status, _) = fixture.get("/playlist/items?target=m3u-target&cluster=live&fields=epg_status").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    fixture.write_m3u_epg("m3u-target", &[epg("known", Some("Known"), vec![])]);
    let (_, body) = fixture.get("/playlist/items?target=m3u-target&cluster=live&epg_status=matched").await;
    assert_eq!(ids(&body, "items"), vec!["Matched"]);
    let (_, body) = fixture
        .get("/playlist/items?target=m3u-target&cluster=live&epg_status=unmatched&epg_status=missing_id&fields=name&fields=epg_status")
        .await;
    assert_eq!(body["total_matched"], 3);
    let statuses: Vec<(&str, &str)> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["name"].as_str().unwrap(), item["epg_status"].as_str().unwrap()))
        .collect();
    assert_eq!(statuses, vec![("Unmatched", "unmatched"), ("Missing", "missing_id"), ("Blank", "missing_id")]);
}

#[tokio::test]
async fn items_epg_status_uses_the_target_output_casing() {
    let fixture = fixture();
    fixture.write_xtream_target(
        "folded-target",
        XtreamCluster::Live,
        vec![xtream_item(1, 1, "Upper", "g", Some("BBC.One")), xtream_item(2, 2, "Other", "g", Some("Nope.Id"))],
    );
    // A lowercase-ids target stores its EPG keys folded.
    fixture.write_xtream_epg("folded-target", &[epg("bbc.one", Some("BBC One"), vec![])]);
    let (_, body) = fixture.get("/playlist/items?target=folded-target&cluster=live&epg_status=matched").await;
    assert_eq!(ids(&body, "items"), vec!["Upper"]);
    let (_, body) = fixture.get("/playlist/items?target=folded-target&cluster=live&epg_status=unmatched").await;
    assert_eq!(ids(&body, "items"), vec!["Other"]);
}

#[tokio::test]
async fn epg_channels_rejects_bad_parameters_and_unknown_targets() {
    let fixture = fixture();
    for (uri, status, expected) in [
        ("/playlist/epg/channels", StatusCode::BAD_REQUEST, "Missing target"),
        ("/playlist/epg/channels?target=m3u-target&issue=nope", StatusCode::BAD_REQUEST, "no_programmes, expired"),
        ("/playlist/epg/channels?target=m3u-target&output=strm", StatusCode::BAD_REQUEST, "xtream, m3u"),
        ("/playlist/epg/channels?target=m3u-target&output=xtream", StatusCode::BAD_REQUEST, "no xtream output"),
        ("/playlist/epg/channels?target=ghost", StatusCode::NOT_FOUND, "Unknown target 'ghost'"),
        ("/playlist/epg/channel?target=m3u-target", StatusCode::BAD_REQUEST, "Missing channel_id"),
        ("/playlist/epg/channel?target=m3u-target&channel_id=x&from=soon", StatusCode::BAD_REQUEST, "Invalid from"),
        ("/playlist/epg/channel?target=ghost&channel_id=x", StatusCode::NOT_FOUND, "Unknown target"),
    ] {
        let (actual, body) = fixture.get(uri).await;
        assert_eq!(actual, status, "{uri}");
        assert!(body["error"].as_str().unwrap_or_default().contains(expected), "{uri}: {body}");
    }
}

#[tokio::test]
async fn epg_without_a_database_answers_200_with_an_explaining_note() {
    let fixture = fixture();
    let (status, body) = fixture.get("/playlist/epg/channels?target=m3u-target").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (body["total_matched"].clone(), body["returned"].clone(), body["output"].clone()),
        (0.into(), 0.into(), "m3u".into())
    );
    assert!(body["note"].as_str().unwrap().contains("No EPG database exists yet"), "{body}");

    let (status, body) = fixture.get("/playlist/epg/channel?target=m3u-target&channel_id=x").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["channel"], Value::Null);
    assert!(body["note"].as_str().unwrap().contains("No EPG database exists yet"), "{body}");
}

#[tokio::test]
async fn epg_channels_filter_by_issue_search_and_page() {
    let fixture = fixture();
    fixture.write_m3u_epg("m3u-target", &issue_channels());
    fixture.write_m3u_target("m3u-target", live_items_for_issue_channels());

    let (status, body) = fixture.get("/playlist/epg/channels?target=m3u-target").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["output"], "m3u");
    assert_eq!(body["total_matched"], 6);
    assert_eq!(ids(&body, "channels"), vec!["current", "empty", "expired", "future", "orphan", "untitled"]);
    let by_id = |id: &str| body["channels"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap().clone();
    assert_eq!(by_id("current")["covers_now"], true);
    assert_eq!(by_id("current")["programme_count"], 1);
    assert_eq!(by_id("current")["referenced_by_playlist"], true);
    assert_eq!(by_id("orphan")["referenced_by_playlist"], false);
    assert_eq!(by_id("empty")["first_start"], Value::Null);
    assert!(by_id("expired")["last_stop"].as_str().unwrap().ends_with('Z'));
    assert_eq!(by_id("current")["has_icon"], false);

    for (issue, expected) in [
        ("no_programmes", vec!["empty"]),
        ("expired", vec!["expired"]),
        ("not_current", vec!["empty", "expired", "future"]),
        ("no_title", vec!["untitled"]),
        ("unreferenced", vec!["orphan"]),
    ] {
        let (_, body) = fixture.get(&format!("/playlist/epg/channels?target=m3u-target&issue={issue}")).await;
        assert_eq!(ids(&body, "channels"), expected, "{issue}");
        assert_eq!(body["total_matched"], expected.len(), "{issue}");
    }
    // Several issues combine with OR.
    let (_, body) = fixture
        .get("/playlist/epg/channels?target=m3u-target&issue=expired&issue=unreferenced&issue=no_programmes")
        .await;
    assert_eq!(ids(&body, "channels"), vec!["empty", "expired", "orphan"]);
    // Search matches id or title, case-insensitively, and combines with issues by AND.
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&search=CHANNEL").await;
    assert_eq!(body["total_matched"], 5, "the untitled channel has neither id nor title match");
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&search=exp&issue=not_current").await;
    assert_eq!(ids(&body, "channels"), vec!["expired"]);
    // Paging.
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&offset=4&limit=5").await;
    assert_eq!(ids(&body, "channels"), vec!["orphan", "untitled"]);
    assert_eq!((body["has_more"].clone(), body["total_matched"].clone()), (false.into(), 6.into()));
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&limit=2").await;
    assert_eq!((body["returned"].clone(), body["has_more"].clone()), (2.into(), true.into()));
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&limit=100000").await;
    assert_eq!((body["limit"].clone(), body["limit_clamped"].clone()), (500.into(), true.into()));
}

#[tokio::test]
async fn epg_channels_without_a_stored_playlist_report_unknown_references() {
    let fixture = fixture();
    fixture.write_m3u_epg("m3u-target", &issue_channels());
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&issue=unreferenced").await;
    assert_eq!(body["total_matched"], 0);
    assert!(body["note"].as_str().unwrap().contains("not stored"), "{body}");
    let (_, body) = fixture.get("/playlist/epg/channels?target=m3u-target&limit=1").await;
    assert_eq!(body["channels"][0]["referenced_by_playlist"], Value::Null);
}

#[tokio::test]
async fn epg_channel_returns_programmes_in_the_window_with_truncation() {
    let fixture = fixture();
    let now = Utc::now().timestamp();
    let long_desc = "ä".repeat(900);
    let mut programmes: Vec<EpgProgramme> = (0..5)
        .map(|index| {
            let start = now + index * 600;
            let mut programme = EpgProgramme::new(start, start + 600, "bbc.one".intern());
            programme.title = Some(format!("Show {index}").intern());
            programme.desc = Some(long_desc.as_str().intern());
            programme
        })
        .collect();
    programmes.push(EpgProgramme::new(now + 3 * 86_400, now + 3 * 86_400 + 600, "bbc.one".intern()));
    fixture.write_xtream_epg(
        "folded-target",
        &[EpgChannel { id: "bbc.one".intern(), title: Some("BBC One".intern()), icon: None, programmes }],
    );

    // The lookup is canonicalized with the target's output casing.
    let (status, body) = fixture.get("/playlist/epg/channel?target=folded-target&channel_id=BBC.One").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["output"], "xtream");
    assert_eq!(body["channel"]["id"], "bbc.one");
    assert_eq!(body["channel"]["programme_count"], 6);
    assert_eq!(body["total_in_window"], 5, "default window is now .. now+24h");
    assert_eq!(body["returned"], 5);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["limit"], 100);
    let first = &body["programmes"][0];
    assert_eq!(first["title"], "Show 0");
    assert_eq!(first["desc"].as_str().unwrap().chars().count(), 501);
    assert!(first["desc"].as_str().unwrap().ends_with('…'));
    assert_eq!(first["categories"], serde_json::json!([]));

    let (_, body) = fixture.get("/playlist/epg/channel?target=folded-target&channel_id=bbc.one&limit=2").await;
    assert_eq!(
        (body["returned"].clone(), body["total_in_window"].clone(), body["truncated"].clone()),
        (2.into(), 5.into(), true.into())
    );

    let from = chrono::DateTime::from_timestamp(now + 2 * 86_400, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to = chrono::DateTime::from_timestamp(now + 4 * 86_400, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (_, body) = fixture
        .get(&format!("/playlist/epg/channel?target=folded-target&channel_id=bbc.one&from={from}&to={to}"))
        .await;
    assert_eq!(body["total_in_window"], 1);

    let (status, body) = fixture.get("/playlist/epg/channel?target=folded-target&channel_id=unknown").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("not found"), "{body}");
}

#[tokio::test]
async fn protected_register_mirrors_the_inspection_routes() {
    let fixture = fixture();
    let router = crate::api::endpoints::v1_api_playlist::v1_api_playlist_register_protected(Router::new())
        .with_state(Arc::clone(&fixture.app_state));
    for (uri, expected) in [
        ("/playlist/sources", StatusCode::OK),
        ("/playlist/items?target=m3u-target&cluster=live", StatusCode::OK),
        ("/playlist/items?cluster=live", StatusCode::BAD_REQUEST),
        ("/playlist/epg/channels?target=m3u-target", StatusCode::OK),
        ("/playlist/epg/channel?target=m3u-target&channel_id=x", StatusCode::OK),
        ("/playlist/analytics", StatusCode::OK),
        ("/playlist/analytics/999/unmapped.csv", StatusCode::NOT_FOUND),
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), expected, "{uri}");
    }
}
