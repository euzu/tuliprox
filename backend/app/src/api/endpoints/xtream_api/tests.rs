use super::{
    empty_stream_info_response, get_xtream_player_api_stream_url, is_hls_playback_request, override_live_hls_extension,
    recording_input_matches, resolve_m3u_xtream_timeshift, resolve_xtream_playback_extension,
    xtream_get_catchup_response, xtream_get_short_epg, xtream_player_api_stream, xtream_player_api_stream_with_token,
    ApiStreamContext, ApiStreamRequest, XtreamApiTimeShiftRequest,
};
use crate::{
    api::model::{create_test_app_state, AppState, PlaylistStorage, PlaylistXtreamStorage, UserApiRequest},
    auth::Fingerprint,
    model::{
        Config, ConfigInput, ConfigTarget, Epg, IcsEpgSourceConfig, ProxyUserCredentials, SourcesConfig, TargetOutput,
        XtreamTargetFlagsSet, XtreamTargetOutput,
    },
    processing::parser::ics::parse_ics_file_to_channel,
    repository::{
        epg_write_file, xtream_get_epg_file_path_for_target, xtream_get_storage_path, BPlusTree, VirtualIdRecord,
    },
};
use arc_swap::ArcSwapOption;
use axum::{http::HeaderMap, response::IntoResponse};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use shared::{
    foundation::Filter,
    model::{
        ClusterFlags, InputType, PlaylistItemType, ProcessingOrder, ProxyUserStatus, StreamProperties, UUIDType,
        VideoStreamProperties, VirtualId, XtreamCluster, XtreamPlaylistItem,
    },
    utils::Internable,
};
use std::sync::Arc;
use tempfile::tempdir;

#[test]
fn recording_input_must_match_canonical_playlist_item_input() {
    let expected = ConfigInput { name: "input-a".intern(), ..Default::default() };
    assert!(recording_input_matches(Some(&expected), "input-a"));
    assert!(!recording_input_matches(Some(&expected), "input-b"));
    assert!(recording_input_matches(None, "input-b"));
}

#[test]
fn live_hls_override_is_scoped_to_enabled_xtream_live_requests() {
    let enabled_xtream = provider_input_with_flag(InputType::Xtream, true);
    let disabled_xtream = provider_input_with_flag(InputType::Xtream, false);
    let enabled_m3u = provider_input_with_flag(InputType::M3u, true);
    let cases = [
        (ApiStreamContext::Live, &enabled_xtream, Some(".m3u8"), Some(".ts")),
        (ApiStreamContext::LiveAlt, &enabled_xtream, Some(".m3u8"), Some(".ts")),
        (ApiStreamContext::Live, &disabled_xtream, Some(".m3u8"), Some(".m3u8")),
        (ApiStreamContext::Live, &enabled_m3u, Some(".m3u8"), Some(".m3u8")),
        (ApiStreamContext::Timeshift, &enabled_xtream, Some(".m3u8"), Some(".m3u8")),
        (ApiStreamContext::Movie, &enabled_xtream, Some(".m3u8"), Some(".m3u8")),
        (ApiStreamContext::Series, &enabled_xtream, Some(".m3u8"), Some(".m3u8")),
        (ApiStreamContext::Live, &enabled_xtream, Some(".ts"), Some(".ts")),
        (ApiStreamContext::Live, &enabled_xtream, Some(".mpd"), Some(".mpd")),
        (ApiStreamContext::Live, &enabled_xtream, None, None),
    ];

    for (context, input, extension, expected) in cases {
        assert_eq!(
            override_live_hls_extension(context, input, extension),
            expected,
            "case: {context:?} input_type={:?} ext={extension:?}",
            input.input_type
        );
    }
}

fn provider_input_with_flag(input_type: InputType, disable_hls: bool) -> ConfigInput {
    let dto = shared::model::ConfigInputDto {
        name: "ts-provider".into(),
        input_type,
        url: "http://provider.test".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        options: Some(shared::model::ConfigInputOptionsDto {
            disable_hls_streaming: disable_hls,
            ..shared::model::ConfigInputOptionsDto::default()
        }),
        ..shared::model::ConfigInputDto::default()
    };
    ConfigInput::from(&dto)
}

#[test]
fn hls_failure_response_uses_resolved_playback_extension() {
    let vod = create_test_vod_item("provider://strong/movie/user/pass/813563.mp4", "mp4", PlaylistItemType::Video);
    let hls_vod =
        create_test_vod_item("provider://strong/movie/user/pass/813564.m3u8", "m3u8", PlaylistItemType::Video);

    assert!(!is_hls_playback_request(Some(".m3u8"), &vod));
    assert!(is_hls_playback_request(None, &hls_vod));
}

async fn response_body_text(response: axum::response::Response) -> Result<String, String> {
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|err| format!("failed to read response body: {err}"))?;
    String::from_utf8(body.to_vec()).map_err(|err| format!("response body is not UTF-8: {err}"))
}

fn short_epg_target() -> ConfigTarget {
    ConfigTarget {
        managed_channels: Vec::new(),
        curation: None,
        id: 1,
        enabled: true,
        name: "ics-xtream".to_string(),
        options: None,
        sort: None,
        filter: Filter::default().into(),
        output: vec![TargetOutput::Xtream(XtreamTargetOutput {
            flags: XtreamTargetFlagsSet::new(),
            trakt: None,
            filter: None,
        })],
        rename: None,
        mapping_ids: None,
        mapping: Arc::new(ArcSwapOption::new(None)),
        favourites: None,
        processing_order: ProcessingOrder::default(),
        execution_plan: tuliprox_core::model::TargetExecutionPlan::default(),
        watch: None,
        use_memory_cache: true,
    }
}

fn short_epg_live_item() -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(100),
        provider_id: 0,
        name: "Formula 1".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Sports".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://example.invalid/live.ts".intern(),
        epg_channel_id: Some("f1.calendar".intern()),
        xtream_cluster: XtreamCluster::Live,
        additional_properties: None,
        item_type: PlaylistItemType::Live,
        category_id: 1,
        input_name: "local".intern(),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    }
}

#[tokio::test]
async fn xtream_short_epg_returns_imported_ics_programme_for_matching_channel_id() {
    let dir = tempdir().expect("temp dir");
    let ics_path = dir.path().join("calendar.ics");
    let start = chrono::Utc::now() + chrono::Duration::days(1);
    let stop = start + chrono::Duration::hours(1);
    std::fs::write(
        &ics_path,
        format!(
            concat!(
                "BEGIN:VCALENDAR\r\n",
                "VERSION:2.0\r\n",
                "BEGIN:VEVENT\r\n",
                "UID:f1-qualifying\r\n",
                "DTSTART:{}\r\n",
                "DTEND:{}\r\n",
                "SUMMARY:Formula 1 Qualifying\r\n",
                "DESCRIPTION:Imported from ICS\r\n",
                "END:VEVENT\r\n",
                "END:VCALENDAR\r\n",
            ),
            start.format("%Y%m%dT%H%M%SZ"),
            stop.format("%Y%m%dT%H%M%SZ"),
        ),
    )
    .expect("write ICS fixture");
    let channel = parse_ics_file_to_channel(
        &ics_path,
        "f1.calendar".intern(),
        Some("Formula 1".intern()),
        &IcsEpgSourceConfig::default(),
    )
    .await
    .expect("parse ICS fixture");

    let config = Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Config::default() };
    let target = Arc::new(short_epg_target());
    let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
    std::fs::create_dir_all(&xtream_storage).expect("create xtream storage");
    let epg_path = xtream_get_epg_file_path_for_target(&xtream_storage);
    epg_write_file(
        &target.name,
        &Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![Arc::new(channel)],
            channel_sources: None,
            input_name: None,
        },
        &epg_path,
        &std::collections::HashMap::<Arc<str>, Arc<str>>::new(),
        None,
        &shared::model::EpgOutputOptions::default(),
    )
    .expect("write target EPG");

    let app_state = create_test_app_state(config);
    cache_xtream_test_item(&app_state, &target, short_epg_live_item(), None).await;

    let mut user = ProxyUserCredentials::default();
    user.output_clusters = ClusterFlags::all();
    let response = xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response();
    let body = response_body_text(response).await.expect("read short EPG response");
    let json: serde_json::Value = serde_json::from_str(&body).expect("parse short EPG JSON");
    let decode_listing_text = |field: &str| {
        let encoded = json["epg_listings"][0][field].as_str().expect("encoded listing text");
        String::from_utf8(BASE64_STANDARD.decode(encoded).expect("decode listing text"))
            .expect("decoded listing text is UTF-8")
    };

    assert_eq!(json["epg_listings"][0]["channel_id"], "f1.calendar");
    assert_eq!(decode_listing_text("title"), "Formula 1 Qualifying");
    assert_eq!(decode_listing_text("description"), "Imported from ICS");
}

fn m3u_catchup_item(name: &str, input_name: &str, url: &str, catchup_source: Option<&str>) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(100),
        provider_id: 0,
        name: name.intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Live".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: url.intern(),
        epg_channel_id: Some("f1.calendar".intern()),
        xtream_cluster: XtreamCluster::Live,
        item_type: PlaylistItemType::Live,
        category_id: 1,
        input_name: input_name.intern(),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "100".intern(),
        additional_properties: Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
            name: name.intern(),
            stream_id: 100,
            tv_archive: Some(1),
            tv_archive_duration: Some(7),
            catchup: catchup_source.map(|src| shared::model::CatchupProperties {
                mode: Some("flussonic".intern()),
                source: Some(src.intern()),
                ..shared::model::CatchupProperties::default()
            }),
            ..shared::model::LiveStreamProperties::default()
        }))),
        upstream_user_agent: None,
        input_url: "".intern(),
    }
}

async fn cache_xtream_test_item(
    app_state: &Arc<AppState>,
    target: &Arc<ConfigTarget>,
    item: XtreamPlaylistItem,
    input: Option<ConfigInput>,
) {
    if let Some(input) = input {
        let sources = SourcesConfig {
            inputs: vec![Arc::new(input)],
            sources: vec![crate::model::ConfigSource {
                inputs: vec![item.input_name.clone()],
                targets: vec![target.clone()],
            }],
            ..SourcesConfig::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));
    }
    let mut live = BPlusTree::new();
    live.insert(item.virtual_id.get(), item.clone());
    app_state
        .playlists
        .cache_playlist(
            &target.name,
            PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage::from_trees(
                live,
                BPlusTree::new(),
                BPlusTree::new(),
            ))),
        )
        .await;
    let mut id_mapping = BPlusTree::new();
    id_mapping.insert(
        item.virtual_id,
        VirtualIdRecord::new(
            item.provider_id,
            item.virtual_id,
            PlaylistItemType::Live,
            VirtualId::new(0),
            UUIDType::default(),
        ),
    );
    app_state.playlists.cache_id_mapping(&target.name, id_mapping).await;
}

async fn build_short_epg_app_state(
    dir: &tempfile::TempDir,
    item: XtreamPlaylistItem,
    input: Option<ConfigInput>,
) -> (Arc<AppState>, Arc<ConfigTarget>) {
    let ics_path = dir.path().join("calendar.ics");
    let start = chrono::Utc::now() + chrono::Duration::days(1);
    let stop = start + chrono::Duration::hours(1);
    std::fs::write(
        &ics_path,
        format!(
            concat!(
                "BEGIN:VCALENDAR\r\n",
                "VERSION:2.0\r\n",
                "BEGIN:VEVENT\r\n",
                "UID:f1-qualifying\r\n",
                "DTSTART:{}\r\n",
                "DTEND:{}\r\n",
                "SUMMARY:Formula 1 Qualifying\r\n",
                "DESCRIPTION:Imported from ICS\r\n",
                "END:VEVENT\r\n",
                "END:VCALENDAR\r\n",
            ),
            start.format("%Y%m%dT%H%M%SZ"),
            stop.format("%Y%m%dT%H%M%SZ"),
        ),
    )
    .expect("write ICS fixture");
    let channel = parse_ics_file_to_channel(
        &ics_path,
        "f1.calendar".intern(),
        Some("Formula 1".intern()),
        &IcsEpgSourceConfig::default(),
    )
    .await
    .expect("parse ICS fixture");

    let config = Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Config::default() };
    let target = Arc::new(short_epg_target());
    let xtream_storage = xtream_get_storage_path(&config, &target.name).expect("xtream storage path");
    std::fs::create_dir_all(&xtream_storage).expect("create xtream storage");
    let epg_path = xtream_get_epg_file_path_for_target(&xtream_storage);
    epg_write_file(
        &target.name,
        &Epg {
            priority: 0,
            logo_override: false,
            attributes: None,
            children: vec![Arc::new(channel)],
            channel_sources: None,
            input_name: None,
        },
        &epg_path,
        &std::collections::HashMap::<Arc<str>, Arc<str>>::new(),
        None,
        &shared::model::EpgOutputOptions::default(),
    )
    .expect("write target EPG");

    let app_state = create_test_app_state(config);
    cache_xtream_test_item(&app_state, &target, item, input).await;

    (app_state, target)
}

#[tokio::test]
async fn xtream_short_epg_emits_has_archive_for_m3u_with_bridge_template() {
    let dir = tempdir().expect("temp dir");
    let input = m3u_timeshift_input();
    let item = m3u_catchup_item(
        "Formula 1",
        &input.name,
        "channel/index.m3u8",
        Some("http://provider.example/channel/video-{utc}-{duration}.m3u8"),
    );
    let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
    let mut user = ProxyUserCredentials::default();
    user.output_clusters = ClusterFlags::all();

    let response = xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response();
    let body = response_body_text(response).await.expect("body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");

    assert_eq!(json["epg_listings"][0]["has_archive"], 1);
}

#[tokio::test]
async fn xtream_short_epg_omits_has_archive_for_m3u_with_unsupported_catchup() {
    let dir = tempdir().expect("temp dir");
    let input = m3u_timeshift_input();
    let item = m3u_catchup_item(
        "Formula 1",
        &input.name,
        "channel/index.m3u8",
        Some("http://provider.example/channel/${timestamp}.m3u8"),
    );
    let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
    let mut user = ProxyUserCredentials::default();
    user.output_clusters = ClusterFlags::all();

    let body = response_body_text(xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response())
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");

    assert!(json["epg_listings"][0].get("has_archive").is_none());
}

#[tokio::test]
async fn xtream_short_epg_emits_has_archive_for_native_xtream_input() {
    let dir = tempdir().expect("temp dir");
    let mut input = m3u_timeshift_input();
    input.input_type = InputType::Xtream;
    let item = m3u_catchup_item("Formula 1", &input.name, "channel/index.m3u8", None);
    let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
    let mut user = ProxyUserCredentials::default();
    user.output_clusters = ClusterFlags::all();

    let body = response_body_text(xtream_get_short_epg(&app_state, &user, &target, "100", 4).await.into_response())
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");

    assert_eq!(json["epg_listings"][0]["has_archive"], 1);
}

#[tokio::test]
async fn xtream_catchup_table_is_served_from_local_epg() {
    let dir = tempdir().expect("temp dir");
    // The input points at an unreachable provider: a provider request would fail the call.
    let mut input = m3u_timeshift_input();
    input.input_type = InputType::Xtream;
    input.url = "http://127.0.0.1:9".to_string();
    let item = m3u_catchup_item("Formula 1", &input.name, "channel/index.m3u8", None);
    let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
    let mut user = ProxyUserCredentials::default();
    user.output_clusters = ClusterFlags::all();

    let response = xtream_get_catchup_response(&app_state, &user, &target, "100", "", "", 0).await.into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = response_body_text(response).await.expect("body");
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let listings = json["epg_listings"].as_array().expect("epg listings");

    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0]["id"], "100");
    assert_eq!(listings[0]["epg_id"], "f1.calendar");
    assert_eq!(listings[0]["now_playing"], 0);
    assert_eq!(listings[0]["has_archive"], 0);
    let title = listings[0]["title"].as_str().expect("encoded title");
    assert_eq!(BASE64_STANDARD.decode(title).expect("decode title"), b"Formula 1 Qualifying");
}

#[tokio::test]
async fn xtream_catchup_table_archive_without_duration_uses_one_day_window() {
    let dir = tempdir().expect("temp dir");
    let mut input = m3u_timeshift_input();
    input.input_type = InputType::Xtream;
    input.url = "http://127.0.0.1:9".to_string();
    let mut item = m3u_catchup_item("Formula 1", &input.name, "channel/index.m3u8", None);
    if let Some(StreamProperties::Live(live)) = item.additional_properties.as_mut() {
        live.tv_archive_duration = None;
    }
    let (app_state, target) = build_short_epg_app_state(&dir, item, Some(input)).await;
    let pli = super::xtream_get_item_for_stream_id(100, &app_state.app_config, &app_state.playlists, &target, None)
        .await
        .expect("cached item");
    let classified =
        super::classify_virtual_id(&app_state.app_config, &app_state.playlists, &target, pli.virtual_id).await;

    assert_eq!(super::catchup_archive_days(&app_state, &classified, &pli), Some(1));
}

#[tokio::test]
async fn empty_stream_info_response_uses_vod_object_and_list_shapes() -> Result<(), String> {
    assert_eq!(response_body_text(empty_stream_info_response(XtreamCluster::Video)).await?, "{}");
    assert_eq!(response_body_text(empty_stream_info_response(XtreamCluster::Live)).await?, "[]");
    assert_eq!(response_body_text(empty_stream_info_response(XtreamCluster::Series)).await?, "[]");
    Ok(())
}

fn create_test_vod_item(url: &str, container_extension: &str, item_type: PlaylistItemType) -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(176_141),
        provider_id: 813_563,
        name: "Test".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: Arc::<str>::from(url),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Video,
        additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
            name: "Test".intern(),
            plugin_fingerprint: None,
            imdb: None,
            imdb_rating: None,
            imdb_votes: None,
            effective_rating: None,
            category_id: 0,
            stream_id: 813_563,
            stream_icon: "".intern(),
            direct_source: "".intern(),
            custom_sid: None,
            added: "".intern(),
            container_extension: container_extension.intern(),
            rating: None,
            rating_5based: None,
            stream_type: Some("movie".intern()),
            trailer: None,
            tmdb: None,
            is_adult: 0,
            details: None,
        }))),
        item_type,
        category_id: 0,
        input_name: "strong".intern(),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "813563".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    }
}

#[test]
fn post_query_only_request_prefers_query_when_form_is_missing() {
    let api_query_req = UserApiRequest {
        username: String::from("query-user"),
        password: String::from("query-pass"),
        action: String::from("get_live_streams"),
        ..UserApiRequest::default()
    };

    let api_req = UserApiRequest::merge_query_over_form(&api_query_req, None);

    assert_eq!(api_req.username, "query-user");
    assert_eq!(api_req.password, "query-pass");
    assert_eq!(api_req.action, "get_live_streams");
}

#[test]
fn post_request_prefers_query_over_form() {
    let api_query_req = UserApiRequest {
        username: String::from("query-user"),
        action: String::from("query-action"),
        ..UserApiRequest::default()
    };
    let form_req = UserApiRequest {
        username: String::from("form-user"),
        action: String::from("form-action"),
        ..UserApiRequest::default()
    };

    let api_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));

    assert_eq!(api_req.username, "query-user");
    assert_eq!(api_req.action, "query-action");
}

#[test]
fn timeshift_query_request_prefers_query_when_form_is_missing() {
    let api_query_req = UserApiRequest {
        username: String::from("query-user"),
        password: String::from("query-pass"),
        stream: String::from("42"),
        duration: String::from("60"),
        start: String::from("2024-01-01:00-00"),
        ..UserApiRequest::default()
    };
    let api_req = UserApiRequest::merge_query_over_form(&api_query_req, None);

    assert_eq!(api_req.username, "query-user");
    assert_eq!(api_req.password, "query-pass");
    assert_eq!(api_req.stream, "42");
    assert_eq!(api_req.duration, "60");
    assert_eq!(api_req.start, "2024-01-01:00-00");
}

#[test]
fn timeshift_query_request_prefers_query_over_form() {
    let api_query_req = UserApiRequest {
        username: String::from("query-user"),
        stream: String::from("42"),
        duration: String::from("60"),
        start: String::from("2024-01-01:00-00"),
        ..UserApiRequest::default()
    };
    let form_req = UserApiRequest {
        username: String::from("form-user"),
        stream: String::from("99"),
        duration: String::from("10"),
        start: String::from("form-start"),
        ..UserApiRequest::default()
    };

    let api_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));

    assert_eq!(api_req.username, "query-user");
    assert_eq!(api_req.stream, "42");
    assert_eq!(api_req.duration, "60");
    assert_eq!(api_req.start, "2024-01-01:00-00");
}

#[test]
fn timeshift_path_request_prefers_query_when_form_is_missing() {
    let timeshift_request = XtreamApiTimeShiftRequest {
        username: String::new(),
        password: String::new(),
        duration: String::new(),
        start: String::new(),
        stream_id: String::new(),
    };
    let api_query_req = UserApiRequest {
        username: String::from("query-user"),
        password: String::from("query-pass"),
        stream_id: String::from("42"),
        duration: String::from("60"),
        start: String::from("2024-01-01:00-00"),
        ..UserApiRequest::default()
    };
    let query_req = UserApiRequest::merge_query_over_form(&api_query_req, None);
    let path_req = UserApiRequest {
        username: timeshift_request.username,
        password: timeshift_request.password,
        duration: timeshift_request.duration,
        start: timeshift_request.start,
        stream_id: timeshift_request.stream_id,
        ..UserApiRequest::default()
    };
    let api_req = UserApiRequest::merge_prefer_primary(&path_req, &query_req);

    assert_eq!(api_req.username, "query-user");
    assert_eq!(api_req.password, "query-pass");
    assert_eq!(api_req.stream_id, "42");
    assert_eq!(api_req.duration, "60");
    assert_eq!(api_req.start, "2024-01-01:00-00");
}

#[test]
fn timeshift_path_request_prefers_path_over_query_and_form() {
    let timeshift_request = XtreamApiTimeShiftRequest {
        username: String::from("path-user"),
        password: String::from("path-pass"),
        duration: String::from("120"),
        start: String::from("path-start"),
        stream_id: String::from("7"),
    };
    let api_query_req = UserApiRequest {
        username: String::from("query-user"),
        password: String::from("query-pass"),
        stream_id: String::from("42"),
        duration: String::from("60"),
        start: String::from("query-start"),
        ..UserApiRequest::default()
    };
    let form_req = UserApiRequest {
        username: String::from("form-user"),
        password: String::from("form-pass"),
        stream_id: String::from("99"),
        duration: String::from("10"),
        start: String::from("form-start"),
        ..UserApiRequest::default()
    };

    let query_req = UserApiRequest::merge_query_over_form(&api_query_req, Some(&form_req));
    let path_req = UserApiRequest {
        username: timeshift_request.username,
        password: timeshift_request.password,
        duration: timeshift_request.duration,
        start: timeshift_request.start,
        stream_id: timeshift_request.stream_id,
        ..UserApiRequest::default()
    };
    let api_req = UserApiRequest::merge_prefer_primary(&path_req, &query_req);

    assert_eq!(api_req.username, "path-user");
    assert_eq!(api_req.password, "path-pass");
    assert_eq!(api_req.stream_id, "7");
    assert_eq!(api_req.duration, "120");
    assert_eq!(api_req.start, "path-start");
}

#[test]
fn non_live_playback_extension_prefers_canonical_item_extension_over_client_override() {
    let pli = create_test_vod_item("provider://strong/movie/user/pass/813563.mp4", "mp4", PlaylistItemType::Video);

    assert_eq!(resolve_xtream_playback_extension(Some(".mkv"), &pli).as_deref(), Some(".mp4"));
}

#[test]
fn media_server_xtream_playback_uses_internal_stream_ref_even_with_direct_pms_url_credentials() {
    let input = ConfigInput {
        input_type: InputType::Plex,
        url: "http://pms-user:pms-pass@pms.example.invalid:32400".to_string(),
        ..ConfigInput::default()
    };
    let fallback =
        Arc::<str>::from("media-server://plex/server/rating?part_key=%2Flibrary%2Fparts%2Fredacted%2Ffile.mkv");

    let resolved = get_xtream_player_api_stream_url(&input, ApiStreamContext::Movie, "813563.mkv", &fallback)
        .expect("media-server fallback should be preserved");

    assert_eq!(resolved, fallback);
}

#[test]
fn media_plugin_xtream_playback_uses_internal_stream_ref() {
    let input =
        ConfigInput { input_type: InputType::Plugin, plugin: Some("my-media-plugin".into()), ..ConfigInput::default() };
    let fallback = Arc::<str>::from("media-server://plugin/my-media-plugin/movie-1?stream_key=stream-1");

    let resolved = get_xtream_player_api_stream_url(&input, ApiStreamContext::Movie, "813563.mkv", &fallback)
        .expect("media plugin fallback should be preserved");

    assert_eq!(resolved, fallback);
}

#[test]
fn live_playback_extension_keeps_client_requested_extension() {
    let mut pli = create_test_vod_item("provider://strong/live/user/pass/813563.ts", "ts", PlaylistItemType::Live);
    pli.xtream_cluster = XtreamCluster::Live;

    assert_eq!(resolve_xtream_playback_extension(Some(".m3u8"), &pli).as_deref(), Some(".m3u8"));
}

#[test]
fn catchup_playback_extension_preserves_requested_adaptive_extension_for_underlying_live_item() {
    let mut pli = create_test_vod_item("provider://strong/live/user/pass/813563.ts", "ts", PlaylistItemType::Live);
    pli.xtream_cluster = XtreamCluster::Live;

    assert_eq!(resolve_xtream_playback_extension(Some(".m3u8"), &pli).as_deref(), Some(".m3u8"));
}

fn m3u_timeshift_input() -> ConfigInput {
    ConfigInput {
        id: 0,
        name: "m3u-flussonic".intern(),
        input_type: InputType::M3u,
        url: "http://provider.example".to_string(),
        username: Some("alice".to_string()),
        password: Some("secret".to_string()),
        ..ConfigInput::default()
    }
}

fn m3u_timeshift_item() -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(100),
        provider_id: 0,
        name: "Live TV".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "Live".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "channel/index.m3u8".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Live,
        additional_properties: Some(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
            name: "Live TV".intern(),
            stream_id: 100,
            tv_archive: Some(1),
            tv_archive_duration: Some(7),
            catchup: Some(shared::model::CatchupProperties {
                mode: Some("flussonic".intern()),
                source: Some("http://provider.example/channel/video-{utc}-{duration}.m3u8".intern()),
                ..shared::model::CatchupProperties::default()
            }),
            ..shared::model::LiveStreamProperties::default()
        }))),
        item_type: PlaylistItemType::Live,
        category_id: 1,
        input_name: "m3u-flussonic".intern(),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "100".intern(),
        upstream_user_agent: None,
        input_url: "".intern(),
    }
}

#[test]
fn m3u_timeshift_resolver_returns_resolved_url_for_flussonic_template() {
    let input = m3u_timeshift_input();
    let item = m3u_timeshift_item();

    let resolved = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00")
        .expect("resolver ok")
        .expect("M3U input handled by bridge");

    assert_eq!(resolved.url, "http://provider.example/channel/video-1704067200-3600.m3u8");
    assert_ne!(resolved.discriminator.len(), 0);
}

#[test]
fn m3u_timeshift_resolver_distinguishes_time_windows() {
    let input = m3u_timeshift_input();
    let item = m3u_timeshift_item();

    let first = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00").expect("first ok").expect("some");
    let second = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:01-00").expect("second ok").expect("some");

    assert_ne!(first.url, second.url);
    assert_ne!(first.discriminator, second.discriminator);
    let fp = Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:0".parse().unwrap());
    assert_ne!(
        crate::api::api_utils::create_m3u_catchup_session_key(&fp, "alice", 100, &first.discriminator),
        crate::api::api_utils::create_m3u_catchup_session_key(&fp, "alice", 100, &second.discriminator),
    );
}

#[test]
fn m3u_timeshift_resolver_rejects_missing_catchup_metadata() {
    let mut item = m3u_timeshift_item();
    if let Some(StreamProperties::Live(live)) = item.additional_properties.as_mut() {
        live.catchup = None;
    }
    let input = m3u_timeshift_input();

    let err =
        resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00").expect_err("missing catchup must error");
    assert_eq!(err.kind(), shared::error::ErrorKind::ApiXtream);
}

#[test]
fn m3u_timeshift_resolver_returns_none_for_non_m3u_input() {
    let mut input = m3u_timeshift_input();
    input.input_type = InputType::Xtream;
    let item = m3u_timeshift_item();

    let resolved = resolve_m3u_xtream_timeshift(&input, &item, "60/2024-01-01:00-00")
        .expect("non-M3U inputs are skipped, not errored");
    assert!(resolved.is_none(), "non-M3U input must be skipped by helper");
}

#[test]
fn m3u_timeshift_stream_url_uses_resolved_archive_for_credential_bearing_input() {
    let input = m3u_timeshift_input();
    let resolved_url: Arc<str> = Arc::from("http://provider.example/channel/video-1704067200-3600.m3u8");

    let url =
        get_xtream_player_api_stream_url(&input, ApiStreamContext::Timeshift, "60/2024-01-01:00-00", &resolved_url)
            .expect("resolved archive URL must win for M3U timeshift");

    assert_eq!(url, resolved_url);
    assert!(!url.contains("alice"));
    assert!(!url.contains("secret"));
}

fn setup_flussonic_archive_timeshift_fixture() -> (ConfigInput, i64, String) {
    let profile = Arc::new(crate::model::ConfigProvider::from(&shared::model::ConfigProviderDto {
        name: "cdn".into(),
        urls: vec!["https://a.example.com/media".into(), "https://b.example.com/media".into()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let input = ConfigInput {
        id: 1,
        name: "m3u-flussonic".intern(),
        input_type: InputType::M3u,
        url: "http://playlist.example.com/list.m3u".to_string(),
        playback_provider: Some("cdn".into()),
        provider_configs: Some(vec![profile]),
        ..ConfigInput::default()
    };
    let start = chrono::Utc::now().timestamp() - 86400;
    let action_path = format!("120/{start}");
    (input, start, action_path)
}

fn bounded_flussonic_timeshift_item(
    live_url: &str,
    stored_source: Option<&str>,
    max_duration_secs: Option<u32>,
) -> XtreamPlaylistItem {
    let mut item = m3u_timeshift_item();
    item.url = live_url.intern();
    if let Some(StreamProperties::Live(live)) = item.additional_properties.as_mut() {
        live.catchup = Some(shared::model::CatchupProperties {
            mode: Some("fs".intern()),
            source: stored_source.map(Internable::intern),
            flussonic_archive_max_duration_secs: max_duration_secs,
            ..shared::model::CatchupProperties::default()
        });
    }
    item
}

#[test]
fn m3u_timeshift_resolver_follows_playback_provider_for_generated_archive() {
    let (mut input, start, action_path) = setup_flussonic_archive_timeshift_fixture();

    let cases = [
        (
            "case a: stored mirror B template",
            true,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            Some(1800),
            format!("provider://cdn/Channel_A/archive-{start}-1800.m3u8?sig=%2B%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
        (
            "case b: stored mirror A template",
            true,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://a.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            Some(1800),
            format!("provider://cdn/Channel_A/archive-{start}-1800.m3u8?sig=%2B%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
        (
            "case c: explicit source without marker",
            true,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            None,
            format!("https://b.example.com/media/Channel_A/archive-{start}-7200.m3u8?sig=%2B%2F%3D"),
            None,
        ),
        (
            "case d: foreign host",
            true,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://foreign.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            Some(1800),
            format!("https://foreign.example.com/media/Channel_A/archive-{start}-1800.m3u8?sig=%2B%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
        (
            "case e: other channel",
            true,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://b.example.com/media/Channel_B/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            Some(1800),
            format!("https://b.example.com/media/Channel_B/archive-{start}-1800.m3u8?sig=%2B%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
        (
            "case f: other query encoding",
            true,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2b%2F%3D",
            Some(1800),
            format!("https://b.example.com/media/Channel_A/archive-{start}-1800.m3u8?sig=%2b%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
        (
            "case g: unmapped live item",
            true,
            "https://b.example.com/media/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            Some(1800),
            format!("https://b.example.com/media/Channel_A/archive-{start}-1800.m3u8?sig=%2B%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
        (
            "case h: playback_provider disabled",
            false,
            "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
            "https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D",
            Some(1800),
            format!("https://b.example.com/media/Channel_A/archive-{start}-1800.m3u8?sig=%2B%2F%3D"),
            Some(format!("archive|{start}|1800")),
        ),
    ];

    for (label, provider_enabled, live_url, stored_source, marker, expected_url, expected_discriminator) in cases {
        input.playback_provider = provider_enabled.then(|| "cdn".into());
        let item = bounded_flussonic_timeshift_item(live_url, Some(stored_source), marker);
        let resolved = resolve_m3u_xtream_timeshift(&input, &item, &action_path)
            .expect("resolver ok")
            .expect("M3U input handled by bridge");
        assert_eq!(resolved.url, expected_url, "{label}: url mismatch");
        if let Some(expected_disc) = expected_discriminator {
            assert_eq!(resolved.discriminator, expected_disc, "{label}: discriminator mismatch");
        } else {
            assert_eq!(resolved.discriminator, shared::utils::short_hash(&expected_url), "{label}: hash mismatch");
        }
    }

    input.playback_provider = Some("cdn".into());
    let item = bounded_flussonic_timeshift_item(
        "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
        Some("https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D"),
        Some(1800),
    );
    assert!(resolve_m3u_xtream_timeshift(&input, &item, "0/2024-01-01:00-00").is_err(), "zero duration must fail");
    assert!(resolve_m3u_xtream_timeshift(&input, &item, "60/9999999999").is_err(), "future timestamp must fail");
}

#[tokio::test]
async fn m3u_timeshift_supports_archive_accepts_mapped_explicit_and_unmapped_items() {
    let dir = tempdir().expect("temp dir");
    let config = Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Config::default() };
    let app_state = create_test_app_state(config);
    let (input, _, _) = setup_flussonic_archive_timeshift_fixture();
    let target = Arc::new(short_epg_target());
    let sources = SourcesConfig {
        inputs: vec![Arc::new(input)],
        sources: vec![crate::model::ConfigSource { inputs: vec!["m3u-flussonic".intern()], targets: vec![target] }],
        ..SourcesConfig::default()
    };
    app_state.app_config.sources.store(Arc::new(sources));

    let mapped = bounded_flussonic_timeshift_item(
        "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
        Some("https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D"),
        Some(1800),
    );
    let explicit = bounded_flussonic_timeshift_item(
        "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
        Some("https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D"),
        None,
    );
    let unmapped = bounded_flussonic_timeshift_item(
        "https://b.example.com/media/Channel_A/mono.m3u8?sig=%2B%2F%3D",
        Some("https://b.example.com/media/Channel_A/archive-{utc}-{duration}.m3u8?sig=%2B%2F%3D"),
        Some(1800),
    );

    for (label, item) in [("mapped", mapped), ("explicit", explicit), ("unmapped", unmapped)] {
        assert!(super::single_item_supports_archive(&app_state, &item), "{label}: single_item_supports_archive");
        let member = crate::repository::GroupMember {
            member_vid: item.virtual_id,
            provider_id: item.provider_id,
            input_name: item.input_name.clone(),
            url: item.url.clone(),
            cluster: item.xtream_cluster,
            item_type: item.item_type,
            title: item.name.clone(),
            input_stream_id: item.input_stream_id.clone(),
            additional_properties: item.additional_properties.clone(),
            archive: None,
            epg_channel_id: item.epg_channel_id.clone(),
            input_url: item.input_url.clone(),
            upstream_user_agent: None,
        };
        assert!(super::member_supports_archive(&app_state, &member), "{label}: member_supports_archive");
    }
}

async fn run_m3u_timeshift_consumer_mirror_test(
    primary_status: &'static str,
    primary_body: &'static str,
    fallback_status: &'static str,
    fallback_body: &'static str,
) -> (axum::http::StatusCode, String, usize, usize) {
    run_m3u_timeshift_consumer_mirror_test_with_hls_cache(
        primary_status,
        primary_body,
        fallback_status,
        fallback_body,
        false,
    )
    .await
}

async fn spawn_m3u_timeshift_test_origin(
    status_line: &'static str,
    body: &'static str,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
    let addr = listener.local_addr().expect("local addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_task = Arc::clone(&hits);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            hits_task.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0_u8; 2048];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        }
    });
    (format!("http://{addr}/media"), hits)
}

async fn run_m3u_timeshift_consumer_mirror_test_with_hls_cache(
    primary_status: &'static str,
    primary_body: &'static str,
    fallback_status: &'static str,
    fallback_body: &'static str,
    with_hls_cache: bool,
) -> (axum::http::StatusCode, String, usize, usize) {
    use std::sync::atomic::Ordering;

    let (primary_base, primary_hits) = spawn_m3u_timeshift_test_origin(primary_status, primary_body).await;
    let (fallback_base, fallback_hits) = spawn_m3u_timeshift_test_origin(fallback_status, fallback_body).await;

    let dir = tempdir().expect("temp dir");
    let config = Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Config::default() };
    let app_state = create_test_app_state(config);
    if with_hls_cache {
        let mut hls_config = (**app_state.app_config.config.load()).clone();
        hls_config.reverse_proxy =
            Some(crate::model::ReverseProxyConfig::from(&shared::model::ReverseProxyConfigDto {
                hls_cache: Some(app_state.hls.proxy.keep_cache_path(shared::model::HlsCacheConfigDto::default())),
                ..Default::default()
            }));
        app_state.app_config.config.store(Arc::new(hls_config));
    }
    let api_proxy_dto = shared::model::ApiProxyConfigDto {
        server: vec![shared::model::ApiProxyServerInfoDto {
            name: "default".to_string(),
            protocol: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: Some("8901".to_string()),
            timezone: "UTC".to_string(),
            message: "test".to_string(),
            path: None,
        }],
        ..Default::default()
    };
    app_state.app_config.set_api_proxy(crate::model::ApiProxyConfig::from(&api_proxy_dto)).expect("set api proxy");

    let profile = Arc::new(crate::model::ConfigProvider::from(&shared::model::ConfigProviderDto {
        name: "cdn".into(),
        urls: vec![primary_base.into(), fallback_base.clone().into()],
        provider_url_selection_policy: shared::model::ProviderUrlSelectionPolicy::RestartFromFirst,
        dns: None,
    }));
    let input = ConfigInput {
        id: 1,
        name: "m3u-flussonic".intern(),
        input_type: InputType::M3u,
        url: "http://playlist.example.com/list.m3u".to_string(),
        enabled: true,
        max_connections: 5,
        playback_provider: Some("cdn".into()),
        provider_configs: Some(vec![profile]),
        ..ConfigInput::default()
    };
    let stored_mirror_b = format!("{fallback_base}/Channel_A/archive-{{utc}}-{{duration}}.m3u8?sig=%2B%2F%3D");
    let item = bounded_flussonic_timeshift_item(
        "provider://cdn/Channel_A/mono.m3u8?sig=%2B%2F%3D",
        Some(&stored_mirror_b),
        Some(1800),
    );
    let mut target = short_epg_target();
    if with_hls_cache {
        target.options = Some(shared::model::ConfigTargetOptions {
            share_live_streams: shared::model::ConfigTargetShareLiveStreams { hls: true, mpeg_ts: false },
            ..Default::default()
        });
    }
    let target = Arc::new(target);
    cache_xtream_test_item(&app_state, &target, item, Some(input)).await;
    app_state.active_provider.update_config(&app_state.app_config);

    let mut user = ProxyUserCredentials::default();
    user.username = "alice".to_string();
    user.password = "secret".to_string();
    user.max_connections = 5;
    user.output_clusters = ClusterFlags::all();
    user.proxy = shared::model::ProxyType::Reverse(None);
    let user = Arc::new(user);
    let fingerprint = Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:0".parse().unwrap());
    let start = chrono::Utc::now().timestamp() - 86400;
    let action_path = format!("120/{start}");

    let response = xtream_player_api_stream(
        &fingerprint,
        &HeaderMap::new(),
        &app_state,
        &UserApiRequest::default(),
        ApiStreamRequest::from(ApiStreamContext::Timeshift, "alice", "secret", "100.ts", &action_path),
        Some((user, target)),
    )
    .await
    .into_response();

    let status = response.status();
    let body = response_body_text(response).await.expect("response body");
    (status, body, primary_hits.load(Ordering::SeqCst), fallback_hits.load(Ordering::SeqCst))
}

#[tokio::test]
async fn m3u_timeshift_consumer_uses_preferred_provider_mirror_over_stored_mirror_b() {
    let playlist = "#EXTM3U\n#EXTINF:6.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
    let (status, _body, hits_a, hits_b) =
        run_m3u_timeshift_consumer_mirror_test("200 OK", playlist, "200 OK", playlist).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(hits_a, 1, "preferred mirror A must serve the request");
    assert_eq!(hits_b, 0, "stored mirror B must not be hit when A succeeds");
}

#[tokio::test]
async fn m3u_timeshift_consumer_fails_over_from_mirror_a_to_mirror_b() {
    let playlist = "#EXTM3U\n#EXTINF:6.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
    let (status, _body, hits_a, hits_b) =
        run_m3u_timeshift_consumer_mirror_test("502 Bad Gateway", "", "200 OK", playlist).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(hits_a >= 1, "mirror A must be tried first");
    assert_eq!(hits_b, 1, "mirror B must serve the failover request");
}

#[tokio::test]
async fn m3u_timeshift_consumer_serves_hls_archive_through_hls_proxy() {
    let playlist = "#EXTM3U\n#EXTINF:6.0,\nseg0.ts\n#EXT-X-ENDLIST\n";
    let (status, body, _hits_a, hits_b) =
        run_m3u_timeshift_consumer_mirror_test_with_hls_cache("200 OK", playlist, "200 OK", playlist, true).await;

    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(hits_b, 0, "stored mirror B must not be hit when A succeeds");
    assert!(
        body.contains("/hls/shared/live/") && body.contains("/manifest.m3u8"),
        "HLS archive must be served through the HLS session handler (/hls/shared/live/.../manifest.m3u8), got: {body}"
    );
    assert!(
        !body.lines().any(|line| line.trim() == "seg0.ts"),
        "raw relative segment URI must not pass through unrewritten: {body}"
    );
}

#[tokio::test]
async fn expired_user_is_rejected_before_m3u_timeshift_is_resolved() -> Result<(), String> {
    let dir = tempdir().map_err(|err| err.to_string())?;
    let input = m3u_timeshift_input();
    let item = m3u_timeshift_item();
    let config = Config {
        storage_dir: dir.path().to_string_lossy().into_owned(),
        user_access_control: true,
        ..Config::default()
    };
    let app_state = create_test_app_state(config);
    let target = Arc::new(short_epg_target());
    cache_xtream_test_item(&app_state, &target, item, Some(input)).await;

    let mut user = ProxyUserCredentials::default();
    user.username = "expired".to_string();
    user.status = Some(ProxyUserStatus::Expired);
    user.output_clusters = ClusterFlags::all();
    let user = Arc::new(user);
    let addr = "127.0.0.1:0".parse().map_err(|err: std::net::AddrParseError| err.to_string())?;
    let fingerprint = Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), addr);

    let response = xtream_player_api_stream(
        &fingerprint,
        &HeaderMap::new(),
        &app_state,
        &UserApiRequest::default(),
        ApiStreamRequest::from(ApiStreamContext::Timeshift, "expired", "", "100.ts", "invalid"),
        Some((user, target)),
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
    Ok(())
}

#[tokio::test]
async fn xtream_token_stream_rejects_m3u_timeshift_with_bad_request() {
    use crate::auth::create_access_token;

    let dir = tempdir().expect("temp dir");
    let config = Config { storage_dir: dir.path().to_string_lossy().into_owned(), ..Config::default() };
    let app_state = create_test_app_state(config);
    let target = Arc::new(short_epg_target());
    let input = m3u_timeshift_input();
    let item = m3u_timeshift_item();
    cache_xtream_test_item(&app_state, &target, item, Some(input)).await;

    let token = create_access_token(&app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);

    let response = xtream_player_api_stream_with_token(
        &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:0".parse().unwrap()),
        &HeaderMap::new(),
        &app_state,
        target.id,
        ApiStreamRequest::from_access_token(ApiStreamContext::Timeshift, &token, "100.ts", ""),
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

/// Serves a recognisable clip for `maintenance`.
fn enable_maintenance_clip(app_state: &Arc<AppState>) {
    let mut cfg = (**app_state.app_config.config.load()).clone();
    cfg.custom_stream_response_enabled = true;
    app_state.app_config.config.store(Arc::new(cfg));
    let mut packet = vec![0xff_u8; 188];
    packet[..4].copy_from_slice(&[0x47, 0x01, 0x00, 0x10]);
    packet[4..9].copy_from_slice(b"MAINT");
    app_state.app_config.custom_stream_response.store(Some(Arc::new(crate::model::CustomStreamResponse {
        channel_unavailable: None,
        user_connections_exhausted: None,
        provider_connections_exhausted: None,
        low_priority_preempted: None,
        user_account_expired: None,
        panel_api_provisioning: None,
        hls_session_or_lease_expired: None,
        maintenance: Some(crate::api::model::TransportStreamBuffer::new(packet)),
        panel_api_provisioning_hls_segments: Vec::new(),
    })));
}

/// Gives `user` (limited to one connection) a running stream on another channel.
async fn occupy_the_only_connection(app_state: &Arc<AppState>, user: &ProxyUserCredentials) {
    let busy_addr = "127.0.0.1:55401".parse().expect("addr");
    let busy = Fingerprint::new("fp-busy".to_string(), "127.0.0.1".to_string(), busy_addr);
    app_state.connection_manager.add_connection(&busy_addr).await;
    app_state
        .active_users
        .create_user_session(crate::api::model::CreateUserSessionParams {
            user,
            session_token: "busy-session",
            virtual_id: 7,
            provider: "provider-b",
            stream_url: "http://other.example/live/7.ts",
            addr: &busy_addr,
            connection_permission: shared::model::UserConnectionPermission::Allowed,
            connection_kind: Some(crate::api::model::ConnectionKind::Normal),
            socket_bound: false,
        })
        .await;
    let channel = shared::model::StreamChannel {
        target_id: 1,
        virtual_id: 7,
        group_virtual_id: None,
        provider_id: 7,
        input_name: "provider-b".intern(),
        item_type: PlaylistItemType::Live,
        cluster: XtreamCluster::Live,
        group: "Live".intern(),
        title: "Busy".intern(),
        url: "http://other.example/live/7.ts".into(),
        shared: false,
        shared_joined_existing: None,
        shared_stream_id: None,
        technical: None,
        epg_channel_id: None,
        epg_reference_ts: None,
        upstream_user_agent: None,
    };
    app_state
        .connection_manager
        .update_connection(crate::api::model::ConnectionParams {
            meter_uid: 401,
            username: "busy",
            max_connections: 1,
            soft_connections: 0,
            connection_kind: crate::api::model::ConnectionKind::Normal,
            priority: 0,
            soft_priority: 0,
            fingerprint: &busy,
            provider: "provider-b".intern(),
            stream_channel: &channel,
            user_agent: std::borrow::Cow::Borrowed("player/1.0"),
            session_token: Some("busy-session"),
        })
        .await;
}

#[tokio::test]
async fn maintenance_answers_before_admission_for_an_exhausted_user() {
    // An exhausted user asking for an input in maintenance gets the maintenance clip, and the
    // request never reaches admission (no connection-denied event, no eviction).
    let dir = tempdir().expect("temp dir");
    let config = Config {
        storage_dir: dir.path().to_string_lossy().into_owned(),
        user_access_control: true,
        ..Config::default()
    };
    let app_state = create_test_app_state(config);
    enable_maintenance_clip(&app_state);
    let target = Arc::new(short_epg_target());
    let mut item = short_epg_live_item();
    item.provider_id = 100;
    item.input_name = "provider_a".intern();
    item.url = "http://127.0.0.1:1/live/user/pass/100.ts".intern();
    let input = ConfigInput {
        id: 1,
        name: "provider_a".intern(),
        input_type: InputType::Xtream,
        url: "http://127.0.0.1:1".to_string(),
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        enabled: true,
        max_connections: 5,
        ..ConfigInput::default()
    };
    cache_xtream_test_item(&app_state, &target, item, Some(input)).await;
    app_state.maintenance.add("provider_a", None, 1).await.expect("maintenance");

    let mut user = ProxyUserCredentials::default();
    user.username = "busy".to_string();
    user.password = "busy".to_string();
    user.max_connections = 1;
    user.soft_connections = 0;
    user.proxy = shared::model::ProxyType::Reverse(None);
    user.output_clusters = ClusterFlags::all();
    occupy_the_only_connection(&app_state, &user).await;

    let addr = "127.0.0.1:55402".parse().expect("addr");
    let fingerprint = Fingerprint::new("fp-new".to_string(), "127.0.0.1".to_string(), addr);
    let response = xtream_player_api_stream(
        &fingerprint,
        &HeaderMap::new(),
        &app_state,
        &UserApiRequest::default(),
        ApiStreamRequest::from(ApiStreamContext::LiveAlt, "busy", "busy", "100.ts", ""),
        Some((Arc::new(user), target)),
    )
    .await
    .into_response();

    assert_eq!(response.status(), axum::http::StatusCode::OK, "the maintenance clip answers");
    let body = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut body = response.into_body();
        let frame = http_body_util::BodyExt::frame(&mut body).await;
        frame.and_then(Result::ok).and_then(|frame| frame.into_data().ok()).unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    assert!(body.windows(5).any(|window| window == b"MAINT"), "the maintenance clip is served");
    let denied = app_state
        .active_users
        .events()
        .stats()
        .emitted()
        .into_iter()
        .find_map(|(kind, count)| (kind == shared::model::EventKind::ConnectionDenied).then_some(count))
        .unwrap_or(0);
    assert_eq!(denied, 0, "the request never reached admission");
}

#[tokio::test]
async fn xtream_token_stream_routes_group_virtual_id_to_group_playback() {
    use crate::{
        api::{
            api_utils::tests::group_playback_support::GroupPlaybackEnv,
            endpoints::xtream_api::{xtream_player_api_stream_with_token, ApiStreamContext, ApiStreamRequest},
        },
        auth::create_access_token,
    };
    use axum::{http::HeaderMap, response::IntoResponse};
    use tuliprox_core::model::Fingerprint;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("korath", 2, 95)
        .with_qos("solis", 1, 20)
        .build()
        .await;

    let token =
        create_access_token(&env.app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let stream_id = format!("{}.ts", env.group_vid);
    let response = xtream_player_api_stream_with_token(
        &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:12345".parse().unwrap()),
        &HeaderMap::new(),
        &env.app_state,
        env.target.id,
        ApiStreamRequest::from_access_token(ApiStreamContext::Live, &token, &stream_id, ""),
    )
    .await
    .into_response();

    assert_eq!(response.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1, "group playback should pick preferred member korath");
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn playlist_webplayer_stream_routes_group_virtual_id() {
    use crate::{
        api::{
            api_utils::tests::group_playback_support::GroupPlaybackEnv,
            endpoints::v1_api_playlist::v1_api_playlist_register_public,
        },
        auth::create_access_token,
    };
    use axum::{body::Body, http::Request};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .with_qos("korath", 2, 95)
        .with_qos("solis", 1, 20)
        .build()
        .await;

    let token =
        create_access_token(&env.app_state.app_config.access_token_secret, 60, crate::auth::scope::INTERNAL_PLAYER);
    let stream_id = format!("{}.ts", env.group_vid);
    let router = v1_api_playlist_register_public(axum::Router::new()).with_state(Arc::clone(&env.app_state));

    let uri = format!("/playlist/webplayer/{token}/{}/live/{stream_id}", env.target.id);
    let mut req = Request::builder().uri(uri).body(Body::empty()).expect("request builder");
    req.extensions_mut().insert(axum::extract::ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
    let response = router.oneshot(req).await.expect("endpoint response");

    assert_eq!(response.status(), 200);
    assert_eq!(env.origin_hits("korath"), 1, "webplayer token route should pick preferred member korath");
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn filtered_xtream_categories_keep_one_generation_during_replacement() {
    use crate::repository::{
        persist_playlist, xtream_category_view_lock_path, xtream_get_file_path, TargetPlaylistPersistOptions,
    };
    use shared::model::{PlaylistGroup, PlaylistItem, PlaylistItemHeader};
    use std::time::Duration;

    let directory = tempdir().unwrap();
    let state = create_test_app_state(Config {
        storage_dir: directory.path().to_string_lossy().into_owned(),
        ..Default::default()
    });
    let target = ConfigTarget { use_memory_cache: false, ..short_epg_target() };
    let group = |name: &str, id: u32| {
        let name = name.intern();
        let mut header = PlaylistItemHeader {
            id: id.to_string().intern(),
            name: name.clone(),
            title: name.clone(),
            group: name.clone(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            url: format!("http://fixture.invalid/live/{id}").intern(),
            ..Default::default()
        };
        header.gen_uuid();
        PlaylistGroup { id, title: name, channels: vec![PlaylistItem { header }], xtream_cluster: XtreamCluster::Live }
    };
    persist_playlist(
        &state.app_config,
        &mut [group("Old category", 101)],
        None,
        &target,
        None,
        TargetPlaylistPersistOptions::default(),
    )
    .await
    .unwrap();

    let mut user = ProxyUserCredentials::default();
    user.username = "filtered-fixture".into();
    user.t_filter = Some(Arc::new(shared::foundation::get_filter(r#"Name ~ "category$""#, None).unwrap()));
    user.output_clusters = ClusterFlags::all();
    let read_categories = |state: Arc<AppState>, target: ConfigTarget, user: ProxyUserCredentials| async move {
        let response = super::xtream_player_api_handle_content_action(
            &state,
            &target,
            crate::model::XC_ACTION_GET_LIVE_CATEGORIES,
            None,
            &user,
        )
        .await
        .unwrap()
        .into_response();
        let json: serde_json::Value = serde_json::from_str(&response_body_text(response).await.unwrap()).unwrap();
        let categories = json.as_array().unwrap();
        assert_eq!(categories.len(), 1);
        (
            categories[0]["category_name"].as_str().unwrap().to_owned(),
            categories[0]["category_id"].as_str().unwrap().to_owned(),
        )
    };

    let storage = xtream_get_storage_path(&state.app_config.config.load(), &target.name).unwrap();
    let locks = &state.app_config.file_locks;
    let visibility_block = locks.write_lock(&xtream_get_file_path(&storage, XtreamCluster::Live)).await;
    let category_view = xtream_category_view_lock_path(&storage);
    let reader = tokio::spawn(read_categories(state.clone(), target.clone(), user.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while locks.try_write_lock(&category_view).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reader must hold the view lock while its stream scan is blocked");

    let writer = tokio::spawn({
        let (app, target) = (state.app_config.clone(), target.clone());
        async move {
            let options = TargetPlaylistPersistOptions { replace_target_contents: true, ..Default::default() };
            persist_playlist(&app, &mut [group("New category", 102)], None, &target, None, options).await.unwrap();
        }
    });
    let config = state.app_config.config.load();
    let target_path = crate::repository::get_target_storage_path(&config, &target.name).unwrap();
    let mapping = crate::repository::get_target_id_mapping_file(&target_path);
    tokio::time::timeout(Duration::from_secs(5), async {
        while locks.try_write_lock(&mapping).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replacement must enter staging while the reader is held");
    assert!(!writer.is_finished());
    drop(visibility_block);
    let old = reader.await.unwrap();
    writer.await.unwrap();
    let new = read_categories(state, target, user).await;
    assert_eq!(old.0, "Old category");
    assert_eq!(new.0, "New category");
    assert_ne!(old.1, new.1, "mixed generations would yield an empty intersection");
}

fn set_xtream_test_user_filter(
    env: &crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv,
    filter_expr: Option<&str>,
) {
    let mut proxy = env.app_state.app_config.api_proxy.load_full().expect("api proxy").as_ref().clone();
    let mut updated_user = proxy.user[0].credentials[0].as_ref().clone();
    updated_user.t_filter =
        filter_expr.map(|expr| Arc::new(shared::foundation::get_filter(expr, None).expect("valid filter")));
    proxy.user[0].credentials = vec![Arc::new(updated_user)];
    env.app_state.app_config.api_proxy.store(Some(Arc::new(proxy)));
}

#[tokio::test]
async fn xtream_content_filter_hiding_group_entry_denies_member_vid_and_group_vid() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let member_1_vid = env.group_record.members[1].member_vid.get();

    // Group entry has Group="General". Filter Group != "General" hides the group entry.
    set_xtream_test_user_filter(&env, Some(r#"Group != "General""#));

    // 1. Member VID request must be denied with the filtered item status (502 ChannelUnavailable).
    let member_res = env.get_xtream_live(member_1_vid).await;
    assert_eq!(member_res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 0);

    // 2. Group VID request must also be denied.
    let group_res = env.get_xtream_live(env.group_vid).await;
    assert_eq!(group_res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 0);
}

#[tokio::test]
async fn xtream_content_filter_allowing_group_entry_serves_member_vid_starting_on_member() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let member_1_vid = env.group_record.members[1].member_vid.get();

    // Group entry has Group="General". Synthesized member item has Group="zdf hd".
    // Filter Group="General" matches the group entry but would NOT match the synthesized member item.
    set_xtream_test_user_filter(&env, Some(r#"Group = "General""#));

    let member_res = env.get_xtream_live(member_1_vid).await;
    assert_eq!(member_res.status(), StatusCode::OK);
    assert_eq!(env.origin_hits("korath"), 1, "member VID must start on member 1 (korath)");
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn xtream_content_filter_hls_member_vid_checks_group_entry() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).hls().build().await;
    let member_1_vid = env.group_record.members[1].member_vid.get();

    // 1. Filter hiding group entry denies member VID HLS request with 404 NOT_FOUND.
    set_xtream_test_user_filter(&env, Some(r#"Group != "General""#));
    let denied_hls = env.get_hls_manifest(member_1_vid).await;
    assert_eq!(denied_hls.status(), StatusCode::NOT_FOUND);

    // 2. Filter allowing group entry serves member VID HLS request with 200 OK.
    set_xtream_test_user_filter(&env, Some(r#"Group = "General""#));
    let allowed_hls = env.get_hls_manifest(member_1_vid).await;
    assert_eq!(allowed_hls.status(), StatusCode::OK);
}

#[tokio::test]
async fn xtream_content_filter_none_serves_member_vid() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"]).build().await;
    let member_1_vid = env.group_record.members[1].member_vid.get();

    // User without filter
    set_xtream_test_user_filter(&env, None);

    let member_res = env.get_xtream_live(member_1_vid).await;
    assert_eq!(member_res.status(), StatusCode::OK);
    assert_eq!(env.origin_hits("korath"), 1, "member VID must start on member 1 (korath)");
    assert_eq!(env.origin_hits("solis"), 0);
}

#[tokio::test]
async fn xtream_timeshift_content_filter_hiding_group_entry_denies_group_vid() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .with_member_archives(&[Some(7), Some(7)])
        .build()
        .await;

    // Filter hiding the group entry (group entry has Group="General")
    set_xtream_test_user_filter(&env, Some(r#"Group != "General""#));

    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let res = env.get_xtream_timeshift(env.group_vid, "60", &now_minus_1_day).await;

    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 0);
}

#[tokio::test]
async fn xtream_timeshift_content_filter_matching_group_category_serves_group_vid() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .with_member_archives(&[Some(7), Some(7)])
        .build()
        .await;

    // Filter matching the group entry's category ("General") but not member rows ("zdf hd")
    set_xtream_test_user_filter(&env, Some(r#"Group = "General""#));

    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let res = env.get_xtream_timeshift(env.group_vid, "60", &now_minus_1_day).await;

    assert_eq!(res.status(), StatusCode::OK);
    assert!(env.origin_hits("solis") + env.origin_hits("korath") >= 1);
}

#[tokio::test]
async fn xtream_catchup_content_filter_hiding_group_entry_denies_member_vid() {
    use crate::api::api_utils::tests::group_playback_support::GroupPlaybackEnv;
    use axum::http::StatusCode;

    let env = GroupPlaybackEnv::builder(&["solis", "korath"])
        .without_plain_channel()
        .with_member_archives(&[Some(7), Some(7)])
        .build()
        .await;

    let member_1_vid = env.group_record.members[1].member_vid.get();

    // Filter hiding the group entry (group entry has Group="General")
    set_xtream_test_user_filter(&env, Some(r#"Group != "General""#));

    let now_minus_1_day = (chrono::Utc::now() - chrono::Duration::days(1)).format("%Y-%m-%d:%H-%M").to_string();
    let res = env.get_xtream_timeshift(member_1_vid, "60", &now_minus_1_day).await;

    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(env.origin_hits("solis"), 0);
    assert_eq!(env.origin_hits("korath"), 0);
}

mod recording_vod {
    use super::*;
    use crate::api::{
        endpoints::xtream_api::xtream_player_api,
        model::{RecordingTask, UserApiRequest},
    };
    use axum::http::StatusCode;
    use shared::model::{
        xtream_const::{RECORDINGS_VOD_CATEGORY_ID, RECORDING_VOD_ID_BASE},
        ApiProxyConfigDto, ApiProxyServerInfoDto, PlayerFeaturesDto, PlayerRecordingLevel, ProxyUserCredentialsDto,
        RecordingKind, RecordingMetadata, RecordingOwner, RecordingSource, RecordingVisibility, TargetUserDto, UserId,
    };
    use tuliprox_dvr::recording::recording_queue::mutate;

    /// 256 bytes with a value equal to their offset, so a range slice is checkable.
    fn recording_bytes() -> Vec<u8> { (0..=255u8).collect() }

    struct RecordingVodEnv {
        _dir: tempfile::TempDir,
        app_state: Arc<AppState>,
        target: Arc<ConfigTarget>,
        vod_id: u32,
    }

    fn api_proxy(recording: PlayerRecordingLevel, alice_filter: Option<&str>) -> crate::model::ApiProxyConfig {
        let dto = ApiProxyConfigDto {
            server: vec![ApiProxyServerInfoDto {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some("8901".to_string()),
                timezone: "UTC".to_string(),
                message: "test".to_string(),
                path: None,
            }],
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![
                    ProxyUserCredentialsDto {
                        username: "alice".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 1,
                        player_features: Some(PlayerFeaturesDto { recording: Some(recording), ..Default::default() }),
                        filter: alice_filter.map(ToString::to_string),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "bob".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 1,
                        player_features: Some(PlayerFeaturesDto {
                            recording: Some(PlayerRecordingLevel::Read),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    ProxyUserCredentialsDto {
                        username: "carol".to_string(),
                        password: "pass".to_string(),
                        ui_enabled: true,
                        max_connections: 1,
                        player_features: Some(PlayerFeaturesDto {
                            recording: Some(PlayerRecordingLevel::None),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
            }],
            ..Default::default()
        };
        let mut api_proxy = crate::model::ApiProxyConfig::from(&dto);
        for target_user in &mut api_proxy.user {
            for credential in &mut target_user.credentials {
                Arc::make_mut(credential).resolve_plan(&std::collections::HashMap::new());
            }
        }
        api_proxy
    }

    async fn seed_task(
        app_state: &Arc<AppState>,
        recording_dir: &std::path::Path,
        owner: &str,
        uuid: &str,
        state: shared::model::RecordingTaskState,
        metadata: RecordingMetadata,
        clip: Option<tuliprox_repository::recording_repository::RecordingClip>,
    ) -> String {
        let relative = format!("{owner}/film.ts");
        let file_path = recording_dir.join(&relative);
        std::fs::create_dir_all(file_path.parent().expect("parent")).expect("create recording dir");
        std::fs::write(&file_path, recording_bytes()).expect("write recording");

        let recording_config = app_state.app_config.config.load().recording().expect("recording config").clone();
        let mut task = RecordingTask::new(
            RecordingKind::Vod,
            "https://provider.example/film.ts",
            "film.ts",
            &recording_config,
            Some(Arc::from("input-a")),
            0,
            metadata,
        )
        .expect("valid task");
        task.uuid = uuid.to_string();
        task.recording.relative_path = Some(relative);
        task.state = state;
        task.finished = state == shared::model::RecordingTaskState::Completed;
        task.size = recording_bytes().len() as u64;
        task.clip = clip;
        let uuid = task.uuid.clone();
        let persisted = crate::api::model::RecordingQueue::to_persisted(&task);
        let partition_is_finished = task.finished;
        mutate(&app_state.recording.queue, move |candidate| {
            if partition_is_finished {
                candidate.finished.push(persisted);
            } else {
                candidate.queue.push(persisted);
            }
            Ok(())
        })
        .await
        .expect("insert recording");
        uuid
    }

    fn media_metadata(owner: &str, visibility: RecordingVisibility, target_id: &str) -> RecordingMetadata {
        RecordingMetadata::new_media(
            RecordingOwner::User(UserId::api(owner)),
            visibility,
            RecordingSource::new(target_id, "42", "input-a").with_cluster(XtreamCluster::Video),
            "Film".to_string(),
        )
    }

    async fn seed_recording(
        app_state: &Arc<AppState>,
        recording_dir: &std::path::Path,
        owner: &str,
        visibility: RecordingVisibility,
        state: shared::model::RecordingTaskState,
    ) -> String {
        seed_task(
            app_state,
            recording_dir,
            owner,
            &format!("{owner}-film"),
            state,
            media_metadata(owner, visibility, "main"),
            None,
        )
        .await
    }

    async fn env_inner(
        recording: PlayerRecordingLevel,
        alice_filter: Option<&str>,
        skip_vod_input: bool,
    ) -> RecordingVodEnv {
        let dir = tempdir().expect("temp dir");
        let recording_dir = dir.path().join("recordings");
        std::fs::create_dir_all(&recording_dir).expect("create recording root");
        let recording_dto = shared::model::RecordingConfigDto {
            enabled: true,
            directory: Some(recording_dir.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            user_config_dir: Some(dir.path().join("user_config").to_string_lossy().into_owned()),
            video: Some(crate::model::VideoConfig {
                extensions: Vec::new(),
                web_search: None,
                recording: Some(crate::model::RecordingConfig::from(&recording_dto)),
            }),
            user_access_control: true,
            ..Config::default()
        };
        let app_state = create_test_app_state(config);
        app_state.app_config.set_api_proxy(api_proxy(recording, alice_filter)).expect("api proxy");
        let target = Arc::new(ConfigTarget { name: "main".to_string(), ..short_epg_target() });
        let inputs = if skip_vod_input {
            vec![Arc::new(ConfigInput {
                id: 1,
                name: "input-a".intern(),
                enabled: true,
                options: Some(crate::model::ConfigInputOptions {
                    flags: crate::model::ConfigInputFlags::SkipVod.into(),
                    ..crate::model::ConfigInputOptions::defaults().clone()
                }),
                ..ConfigInput::default()
            })]
        } else {
            Vec::new()
        };
        let source_inputs = if skip_vod_input { vec!["input-a".intern()] } else { Vec::new() };
        let sources = SourcesConfig {
            inputs,
            sources: vec![crate::model::ConfigSource { inputs: source_inputs, targets: vec![Arc::clone(&target)] }],
            ..SourcesConfig::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        seed_recording(
            &app_state,
            &recording_dir,
            "alice",
            RecordingVisibility::Private,
            shared::model::RecordingTaskState::Completed,
        )
        .await;
        let vod_id = app_state.recording.queue.finished.read().await[0].vod_id;

        RecordingVodEnv { _dir: dir, app_state, target, vod_id }
    }

    async fn env(recording: PlayerRecordingLevel) -> RecordingVodEnv { env_inner(recording, None, false).await }

    fn api_request(action: &str) -> UserApiRequest {
        UserApiRequest {
            username: "alice".to_string(),
            password: "pass".to_string(),
            action: action.to_string(),
            ..UserApiRequest::default()
        }
    }

    /// Caches a movie so the playlist iterator publishes it for the target.
    async fn cache_vod_item(app_state: &Arc<AppState>, target: &Arc<ConfigTarget>, item: XtreamPlaylistItem) {
        let mut video = BPlusTree::new();
        video.insert(item.virtual_id.get(), item.clone());
        app_state
            .playlists
            .cache_playlist(
                &target.name,
                PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage::from_trees(
                    BPlusTree::new(),
                    video,
                    BPlusTree::new(),
                ))),
            )
            .await;
        let mut id_mapping = BPlusTree::new();
        id_mapping.insert(
            item.virtual_id,
            VirtualIdRecord::new(
                item.provider_id,
                item.virtual_id,
                PlaylistItemType::Video,
                VirtualId::new(0),
                UUIDType::default(),
            ),
        );
        app_state.playlists.cache_id_mapping(&target.name, id_mapping).await;
    }

    /// Gives `user` a running stream, so their single connection slot is taken.
    async fn occupy_connection(
        app_state: &Arc<AppState>,
        user: &ProxyUserCredentials,
        session_token: &str,
        virtual_id: u32,
    ) {
        let addr = "127.0.0.1:55411".parse().expect("addr");
        let fingerprint = Fingerprint::new("fp-busy".to_string(), "127.0.0.1".to_string(), addr);
        app_state
            .active_users
            .create_user_session(crate::api::model::CreateUserSessionParams {
                user,
                session_token,
                virtual_id,
                provider: "provider-b",
                stream_url: "http://other.example/live/9.ts",
                addr: &addr,
                connection_permission: shared::model::UserConnectionPermission::Allowed,
                connection_kind: Some(crate::api::model::ConnectionKind::Normal),
                socket_bound: false,
            })
            .await;
        let channel = shared::model::StreamChannel {
            target_id: 1,
            virtual_id,
            group_virtual_id: None,
            provider_id: virtual_id,
            input_name: "provider-b".intern(),
            item_type: PlaylistItemType::Live,
            cluster: XtreamCluster::Live,
            group: "Live".intern(),
            title: "Busy".intern(),
            url: "http://other.example/live/9.ts".into(),
            shared: false,
            shared_joined_existing: None,
            shared_stream_id: None,
            technical: None,
            epg_channel_id: None,
            epg_reference_ts: None,
            upstream_user_agent: None,
        };
        app_state
            .connection_manager
            .update_connection(crate::api::model::ConnectionParams {
                meter_uid: 401,
                username: &user.username,
                max_connections: 1,
                soft_connections: 0,
                connection_kind: crate::api::model::ConnectionKind::Normal,
                priority: 0,
                soft_priority: 0,
                fingerprint: &fingerprint,
                provider: "provider-b".intern(),
                stream_channel: &channel,
                user_agent: std::borrow::Cow::Borrowed("player/1.0"),
                session_token: Some(session_token),
            })
            .await;
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let text = response_body_text(response).await.expect("body");
        serde_json::from_str(&text).expect("json body")
    }

    #[tokio::test]
    async fn vod_categories_include_recordings_for_a_read_user_only() {
        let env = env(PlayerRecordingLevel::Read).await;

        let alice = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5001".parse().expect("addr")),
            api_request(crate::model::XC_ACTION_GET_VOD_CATEGORIES),
            &env.app_state,
        )
        .await
        .into_response();
        assert_eq!(alice.status(), StatusCode::OK);
        let categories = body_json(alice).await;
        let entries = categories.as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["category_id"], RECORDINGS_VOD_CATEGORY_ID.to_string());
        assert_eq!(entries[0]["category_name"], "Recordings");

        let mut carol_request = api_request(crate::model::XC_ACTION_GET_VOD_CATEGORIES);
        carol_request.username = "carol".to_string();
        let carol = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5002".parse().expect("addr")),
            carol_request,
            &env.app_state,
        )
        .await
        .into_response();
        assert_eq!(carol.status(), StatusCode::OK);
        assert_eq!(body_json(carol).await, serde_json::json!([]));
    }

    #[tokio::test]
    async fn vod_streams_lists_the_completed_recording_in_the_recording_category() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_STREAMS);
        request.category_id = RECORDINGS_VOD_CATEGORY_ID.to_string();

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5003".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let entries = body_json(response).await;
        let entries = entries.as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["stream_id"], serde_json::json!(env.vod_id));
        assert_eq!(entries[0]["category_id"], serde_json::json!(RECORDINGS_VOD_CATEGORY_ID.to_string()));
        assert_eq!(entries[0]["container_extension"], serde_json::json!("ts"));
        assert_eq!(entries[0]["name"], serde_json::json!("Film"));
        assert_eq!(env.vod_id, RECORDING_VOD_ID_BASE, "the first recording takes the first reserved id");
    }

    #[tokio::test]
    async fn vod_streams_without_category_appends_the_recording_after_the_playlist() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/77.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(77);
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5004".parse().expect("addr")),
            api_request(crate::model::XC_ACTION_GET_VOD_STREAMS),
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let entries = body_json(response).await;
        let entries = entries.as_array().expect("array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["stream_id"], serde_json::json!(77));
        assert_eq!(entries[1]["stream_id"], serde_json::json!(env.vod_id));
    }

    #[tokio::test]
    async fn vod_streams_with_reserved_category_prefers_provider_category_over_recordings() {
        let env = env(PlayerRecordingLevel::Read).await;

        // Provide a real provider category that collides with the reserved recording category id.
        let config = env.app_state.app_config.config.load();
        let storage =
            crate::repository::ensure_xtream_storage_path(&config, &env.target.name).await.expect("storage path");
        let cat_path = crate::repository::get_collection_path(
            &storage,
            crate::repository::xtream_cluster_category_collection(XtreamCluster::Video),
        );
        let category_entries = serde_json::json!([{
            "category_id": RECORDINGS_VOD_CATEGORY_ID.to_string(),
            "category_name": "Provider Category",
            "parent_id": 0,
        }]);
        std::fs::write(&cat_path, serde_json::to_string(&category_entries).expect("json")).expect("write categories");

        // Seed a provider movie in the colliding category.
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/888.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(888);
        playlist_item.category_id = RECORDINGS_VOD_CATEGORY_ID;
        if let Some(StreamProperties::Video(props)) = &mut playlist_item.additional_properties {
            props.category_id = RECORDINGS_VOD_CATEGORY_ID;
            props.stream_id = 888;
        }
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;

        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_STREAMS);
        request.category_id = RECORDINGS_VOD_CATEGORY_ID.to_string();

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5010".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let entries = body_json(response).await;
        let entries = entries.as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["stream_id"], serde_json::json!(888));
        assert_eq!(entries[0]["category_id"], serde_json::json!(RECORDINGS_VOD_CATEGORY_ID.to_string()));
        assert!(entries.iter().all(|e| e["stream_id"] != serde_json::json!(env.vod_id)));
    }

    #[tokio::test]
    async fn vod_streams_are_unchanged_for_a_user_without_the_feature() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/77.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(77);
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_STREAMS);
        request.username = "carol".to_string();

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5005".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let entries = body_json(response).await;
        let entries = entries.as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["stream_id"], serde_json::json!(77));
    }

    async fn vod_id_of(env: &RecordingVodEnv, uuid: &str) -> u32 {
        env.app_state
            .recording
            .queue
            .finished
            .read()
            .await
            .iter()
            .find(|task| task.uuid == uuid)
            .map(|task| task.vod_id)
            .expect("seeded recording")
    }

    #[tokio::test]
    async fn a_recording_is_found_by_id_only_for_a_user_that_may_see_it() {
        let env = env(PlayerRecordingLevel::Read).await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        seed_recording(
            &env.app_state,
            &recording_dir,
            "bob",
            RecordingVisibility::Private,
            shared::model::RecordingTaskState::Completed,
        )
        .await;
        let bob_vod_id = vod_id_of(&env, "bob-film").await;
        assert_ne!(bob_vod_id, env.vod_id, "each recording carries its own id");

        let alice = env.app_state.app_config.get_target_for_user("alice", "pass").expect("alice").0;
        let bob = env.app_state.app_config.get_target_for_user("bob", "pass").expect("bob").0;

        let own = crate::api::api_utils::visible_recording_by_vod_id(
            &env.app_state,
            &alice,
            env.vod_id,
            &crate::api::endpoints::recording_api::AppRecordingVisibility::new(&env.app_state),
        )
        .await
        .expect("alice sees her own recording");
        assert_eq!(own.uuid, "alice-film");
        assert_eq!(own.vod_id, env.vod_id);

        let bobs = crate::api::api_utils::visible_recording_by_vod_id(
            &env.app_state,
            &alice,
            bob_vod_id,
            &crate::api::endpoints::recording_api::AppRecordingVisibility::new(&env.app_state),
        )
        .await;
        assert_eq!(bobs.map(|entry| entry.uuid), None, "bob's private recording is not alice's");

        let alices = crate::api::api_utils::visible_recording_by_vod_id(
            &env.app_state,
            &bob,
            env.vod_id,
            &crate::api::endpoints::recording_api::AppRecordingVisibility::new(&env.app_state),
        )
        .await;
        assert_eq!(alices.map(|entry| entry.uuid), None, "alice's private recording is not bob's");

        let bobs_own = crate::api::api_utils::visible_recording_by_vod_id(
            &env.app_state,
            &bob,
            bob_vod_id,
            &crate::api::endpoints::recording_api::AppRecordingVisibility::new(&env.app_state),
        )
        .await
        .expect("bob sees his own recording");
        assert_eq!(bobs_own.uuid, "bob-film");
    }

    #[tokio::test]
    async fn another_users_private_recording_is_not_listed() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_STREAMS);
        request.username = "bob".to_string();
        request.category_id = RECORDINGS_VOD_CATEGORY_ID.to_string();

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5013".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let entries = body_json(response).await;
        assert_eq!(entries.as_array().expect("array").len(), 0);
    }

    #[tokio::test]
    async fn vod_info_answers_with_the_recording_stream_id() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_INFO);
        request.vod_id = env.vod_id.to_string();

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5006".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let document = body_json(response).await;
        assert_eq!(document["movie_data"]["stream_id"], serde_json::json!(env.vod_id));
        assert_eq!(document["info"]["name"], serde_json::json!("Film"));
        assert_eq!(document["movie_data"]["container_extension"], serde_json::json!("ts"));
    }

    #[tokio::test]
    async fn movie_path_serves_a_recording_range_with_exact_byte_window() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::RANGE, "bytes=0-99".parse().expect("range header"));

        let response = super::xtream_player_api_stream(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5007".parse().expect("addr")),
            &headers,
            &env.app_state,
            &api_request(""),
            ApiStreamRequest::from(ApiStreamContext::Movie, "alice", "pass", &format!("{}.ts", env.vod_id), ""),
            Some((
                env.app_state.app_config.get_target_for_user("alice", "pass").expect("alice").0,
                Arc::clone(&env.target),
            )),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_RANGE).map(|v| v.to_str().expect("value")),
            Some("bytes 0-99/256")
        );
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE).map(|v| v.to_str().expect("value")),
            Some("video/mp2t")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        assert_eq!(body.as_ref(), &recording_bytes()[..100]);
    }

    async fn movie_response(
        env: &RecordingVodEnv,
        username: &str,
        stream_id: &str,
        headers: HeaderMap,
    ) -> axum::response::Response {
        super::xtream_player_api_stream(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5099".parse().expect("addr")),
            &headers,
            &env.app_state,
            &api_request(""),
            ApiStreamRequest::from(ApiStreamContext::Movie, username, "pass", stream_id, ""),
            Some((
                env.app_state.app_config.get_target_for_user(username, "pass").expect("user").0,
                Arc::clone(&env.target),
            )),
        )
        .await
        .into_response()
    }

    fn fallback_header(response: &axum::response::Response) -> Option<&str> {
        response.headers().get("X-Tuliprox-Fallback").and_then(|value| value.to_str().ok())
    }

    /// The regular movie fallback an unknown id answers with.
    async fn assert_matches_unknown_id_fallback(env: &RecordingVodEnv, username: &str, stream_id: &str) {
        let baseline = movie_response(env, username, "42", HeaderMap::new()).await;
        assert_eq!(baseline.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(fallback_header(&baseline), Some("channel_unavailable"));

        let response = movie_response(env, username, stream_id, HeaderMap::new()).await;
        assert_eq!(response.status(), baseline.status());
        assert_eq!(fallback_header(&response), fallback_header(&baseline));
    }

    #[tokio::test]
    async fn an_unknown_movie_id_below_the_recording_range_answers_with_the_regular_fallback() {
        let env = env(PlayerRecordingLevel::Read).await;
        let with_feature = movie_response(&env, "alice", "42", HeaderMap::new()).await;
        let without_feature = movie_response(&env, "carol", "42", HeaderMap::new()).await;

        assert_eq!(with_feature.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(fallback_header(&with_feature), Some("channel_unavailable"));
        assert_eq!(with_feature.status(), without_feature.status());
        assert_eq!(fallback_header(&with_feature), fallback_header(&without_feature));
    }

    #[tokio::test]
    async fn an_invisible_recording_id_falls_back_like_an_unknown_id() {
        let env = env(PlayerRecordingLevel::Read).await;

        assert_matches_unknown_id_fallback(&env, "bob", &format!("{}.ts", env.vod_id)).await;
    }

    #[tokio::test]
    async fn a_running_recording_is_neither_listed_nor_served() {
        let env = env(PlayerRecordingLevel::Read).await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        seed_recording(
            &env.app_state,
            &recording_dir,
            "alice",
            RecordingVisibility::Private,
            shared::model::RecordingTaskState::Running,
        )
        .await;
        let running_uuid = "alice-film";
        let running_vod_id = env
            .app_state
            .recording
            .queue
            .queue
            .lock()
            .await
            .iter()
            .find(|task| task.uuid == running_uuid)
            .expect("running task")
            .vod_id;

        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_STREAMS);
        request.category_id = RECORDINGS_VOD_CATEGORY_ID.to_string();
        let listed = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5009".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();
        let entries = body_json(listed).await;
        let entries = entries.as_array().expect("array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["stream_id"], serde_json::json!(env.vod_id));

        assert_matches_unknown_id_fallback(&env, "alice", &format!("{running_vod_id}.ts")).await;
    }

    #[tokio::test]
    async fn a_playlist_item_with_the_recording_id_wins_the_info_lookup() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/9.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(env.vod_id);
        playlist_item.name = "Provider Movie".intern();
        playlist_item.title = "Provider Movie".intern();
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_INFO);
        request.vod_id = env.vod_id.to_string();

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5011".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();

        let document = body_json(response).await;
        assert_eq!(document["movie_data"]["stream_id"], serde_json::json!(env.vod_id));
        assert_eq!(document["info"]["name"], serde_json::json!("Provider Movie"));
        assert_eq!(document["movie_data"]["container_extension"], serde_json::json!("mp4"));
    }

    #[tokio::test]
    async fn an_exhausted_user_gets_the_same_rejection_as_for_local_files() {
        let env = env(PlayerRecordingLevel::Read).await;
        let alice = env.app_state.app_config.get_target_for_user("alice", "pass").expect("alice").0;
        occupy_connection(&env.app_state, &alice, "busy-session", 9_999).await;

        let response = super::xtream_player_api_stream(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5012".parse().expect("addr")),
            &HeaderMap::new(),
            &env.app_state,
            &api_request(""),
            ApiStreamRequest::from(ApiStreamContext::Movie, "alice", "pass", &format!("{}.ts", env.vod_id), ""),
            Some((Arc::clone(&alice), Arc::clone(&env.target))),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.headers().get("X-Tuliprox-Rejection").map(|v| v.to_str().expect("value")),
            Some("admission_rejected")
        );
    }
    async fn vod_streams(env: &RecordingVodEnv, username: &str, category_id: Option<u32>) -> Vec<serde_json::Value> {
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_STREAMS);
        request.username = username.to_string();
        if let Some(category_id) = category_id {
            request.category_id = category_id.to_string();
        }
        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5095".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await.as_array().expect("array").clone()
    }

    async fn vod_info(env: &RecordingVodEnv, username: &str, vod_id: u32) -> serde_json::Value {
        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_INFO);
        request.username = username.to_string();
        request.vod_id = vod_id.to_string();
        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5096".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    }

    async fn cache_source_item(env: &RecordingVodEnv, name: &str) {
        let mut item =
            create_test_vod_item("http://provider.example/movie/user/pass/42.mp4", "mp4", PlaylistItemType::Video);
        item.virtual_id = VirtualId::new(42);
        item.name = name.intern();
        item.title = name.intern();
        if let Some(StreamProperties::Video(props)) = &mut item.additional_properties {
            props.name = name.intern();
            props.stream_id = 42;
        }
        cache_vod_item(&env.app_state, &env.target, item).await;
    }

    async fn write_reserved_provider_category(env: &RecordingVodEnv) {
        let config = env.app_state.app_config.config.load();
        let storage =
            crate::repository::ensure_xtream_storage_path(&config, &env.target.name).await.expect("storage path");
        let cat_path = crate::repository::get_collection_path(
            &storage,
            crate::repository::xtream_cluster_category_collection(XtreamCluster::Video),
        );
        let category_entries = serde_json::json!([{
            "category_id": RECORDINGS_VOD_CATEGORY_ID.to_string(),
            "category_name": "Provider Category",
            "parent_id": 0,
        }]);
        std::fs::write(&cat_path, serde_json::to_string(&category_entries).expect("json")).expect("write categories");
    }

    fn write_vod_bouquet(env: &RecordingVodEnv, entries: &serde_json::Value) {
        let config = env.app_state.app_config.config.load();
        let user_dir =
            std::path::PathBuf::from(config.user_config_dir.as_ref().expect("user config dir")).join("alice");
        std::fs::create_dir_all(&user_dir).expect("user dir");
        std::fs::write(user_dir.join("xtream_vod_bouquet.json"), serde_json::to_string(&entries).expect("json"))
            .expect("write bouquet");
    }

    #[tokio::test]
    async fn vod_info_reports_the_recorded_window_and_completion_time() {
        let env = env(PlayerRecordingLevel::Read).await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        let mut metadata = RecordingMetadata::new_live(
            RecordingOwner::User(UserId::api("alice")),
            RecordingVisibility::Private,
            RecordingSource::new("main", "42", "input-a").with_cluster(XtreamCluster::Video),
            1_700_000_000,
            1_700_003_600,
            0,
            0,
        );
        metadata.completed_at = Some(1_700_003_700);
        seed_task(
            &env.app_state,
            &recording_dir,
            "alice",
            "alice-window",
            shared::model::RecordingTaskState::Completed,
            metadata,
            None,
        )
        .await;
        let window_vod_id = vod_id_of(&env, "alice-window").await;

        let document = vod_info(&env, "alice", window_vod_id).await;
        assert_eq!(document["info"]["duration_secs"], serde_json::json!("3600"));
        assert_eq!(document["info"]["duration"], serde_json::json!("01:00:00"));
        assert_eq!(document["movie_data"]["added"], serde_json::json!("1700003700"));

        let listed = vod_streams(&env, "alice", Some(RECORDINGS_VOD_CATEGORY_ID)).await;
        let entry = listed
            .iter()
            .find(|entry| entry["stream_id"] == serde_json::json!(window_vod_id))
            .expect("windowed recording listed");
        assert_eq!(entry["added"], serde_json::json!("1700003700"));
    }

    #[tokio::test]
    async fn movie_recording_with_unsatisfiable_range_answers_416_without_leaking_a_connection() {
        let env = env(PlayerRecordingLevel::Read).await;
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::RANGE, "bytes=256-300".parse().expect("range header"));

        let response = movie_response(&env, "alice", &format!("{}.ts", env.vod_id), headers).await;

        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_RANGE).map(|v| v.to_str().expect("value")),
            Some("bytes */256")
        );
        assert_eq!(env.app_state.active_users.user_connections("alice").await, 0);
    }

    #[tokio::test]
    async fn movie_path_serves_a_clip_header_and_window_with_exact_bytes() {
        let env = env(PlayerRecordingLevel::Read).await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        let header: Vec<u8> = (0..tuliprox_dvr::recording_worker::CLIP_HEADER_LEN)
            .map(|index| u8::try_from(index % 251).expect("byte"))
            .collect();
        let clip = tuliprox_repository::recording_repository::RecordingClip {
            start_ts: 1_700_000_000,
            end_ts: 1_700_000_100,
            start_offset: Some(0),
            end_offset: Some(100),
            header_bytes: Some(header.clone()),
        };
        seed_task(
            &env.app_state,
            &recording_dir,
            "alice",
            "alice-clip",
            shared::model::RecordingTaskState::Completed,
            media_metadata("alice", RecordingVisibility::Private, "main"),
            Some(clip),
        )
        .await;
        let clip_vod_id = vod_id_of(&env, "alice-clip").await;

        let response = movie_response(&env, "alice", &format!("{clip_vod_id}.ts"), HeaderMap::new()).await;

        assert_eq!(response.status(), StatusCode::OK);
        let expected_size = header.len() + 100;
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_LENGTH).map(|v| v.to_str().expect("value")),
            Some(expected_size.to_string().as_str())
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        let mut expected = header;
        expected.extend_from_slice(&recording_bytes()[..100]);
        assert_eq!(body.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn shared_recording_from_the_same_target_passes_the_filter_and_is_listed_and_playable() {
        let env = env_inner(PlayerRecordingLevel::Read, Some(r#"Name ~ "Film""#), false).await;
        cache_source_item(&env, "Film Channel").await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        seed_task(
            &env.app_state,
            &recording_dir,
            "bob",
            "bob-shared",
            shared::model::RecordingTaskState::Completed,
            media_metadata("bob", RecordingVisibility::Shared, "main"),
            None,
        )
        .await;
        let bob_vod_id = vod_id_of(&env, "bob-shared").await;

        let listed = vod_streams(&env, "alice", Some(RECORDINGS_VOD_CATEGORY_ID)).await;
        let ids: Vec<&serde_json::Value> = listed.iter().map(|entry| &entry["stream_id"]).collect();
        assert_eq!(ids, vec![&serde_json::json!(env.vod_id), &serde_json::json!(bob_vod_id)]);

        let response = movie_response(&env, "alice", &format!("{bob_vod_id}.ts"), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        assert_eq!(body.as_ref(), recording_bytes().as_slice());
    }

    #[tokio::test]
    async fn shared_recording_from_another_target_is_not_listed_and_falls_back() {
        let env = env(PlayerRecordingLevel::Read).await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        seed_task(
            &env.app_state,
            &recording_dir,
            "bob",
            "bob-other-target",
            shared::model::RecordingTaskState::Completed,
            media_metadata("bob", RecordingVisibility::Shared, "other"),
            None,
        )
        .await;
        let bob_vod_id = vod_id_of(&env, "bob-other-target").await;

        let listed = vod_streams(&env, "alice", Some(RECORDINGS_VOD_CATEGORY_ID)).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["stream_id"], serde_json::json!(env.vod_id));

        assert_matches_unknown_id_fallback(&env, "alice", &format!("{bob_vod_id}.ts")).await;
    }

    #[tokio::test]
    async fn shared_recording_excluded_by_the_content_filter_is_not_listed_and_falls_back() {
        let env = env_inner(PlayerRecordingLevel::Read, Some(r#"Name ~ "hidden""#), false).await;
        cache_source_item(&env, "Visible Channel").await;
        let recording_dir = std::path::PathBuf::from(
            env.app_state.app_config.config.load().recording().expect("recording").directory.clone(),
        );
        seed_task(
            &env.app_state,
            &recording_dir,
            "bob",
            "bob-filtered",
            shared::model::RecordingTaskState::Completed,
            media_metadata("bob", RecordingVisibility::Shared, "main"),
            None,
        )
        .await;
        let bob_vod_id = vod_id_of(&env, "bob-filtered").await;

        let listed = vod_streams(&env, "alice", Some(RECORDINGS_VOD_CATEGORY_ID)).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["stream_id"], serde_json::json!(env.vod_id));

        assert_matches_unknown_id_fallback(&env, "alice", &format!("{bob_vod_id}.ts")).await;
    }

    #[tokio::test]
    async fn vod_categories_ignore_a_provider_category_hidden_by_the_bouquet_filter() {
        let env = env(PlayerRecordingLevel::Read).await;
        write_reserved_provider_category(&env).await;
        // The bouquet shows a different category, so the colliding provider
        // category is filtered out of the user's list.
        write_vod_bouquet(&env, &serde_json::json!([{"id": 7, "name": "Other"}]));

        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5097".parse().expect("addr")),
            api_request(crate::model::XC_ACTION_GET_VOD_CATEGORIES),
            &env.app_state,
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::OK);
        let categories = body_json(response).await;
        let categories = categories.as_array().expect("array");
        assert!(categories.is_empty(), "the provider owns the reserved id, so no recording category is appended");
    }

    #[tokio::test]
    async fn vod_streams_without_category_omit_recordings_when_the_provider_owns_the_reserved_id() {
        let env = env(PlayerRecordingLevel::Read).await;
        write_reserved_provider_category(&env).await;
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/77.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(77);
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;

        let entries = vod_streams(&env, "alice", None).await;

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["stream_id"], serde_json::json!(77));
    }

    #[tokio::test]
    async fn skip_vod_input_flag_does_not_hide_recordings() {
        let env = env_inner(PlayerRecordingLevel::Read, None, true).await;
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/77.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(77);
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;

        let reserved = vod_streams(&env, "alice", Some(RECORDINGS_VOD_CATEGORY_ID)).await;
        assert_eq!(reserved.len(), 1);
        assert_eq!(reserved[0]["stream_id"], serde_json::json!(env.vod_id));

        let all = vod_streams(&env, "alice", None).await;
        assert_eq!(all.len(), 1, "the playlist part still honours SkipVod");
        assert_eq!(all[0]["stream_id"], serde_json::json!(env.vod_id));

        let document = vod_info(&env, "alice", env.vod_id).await;
        assert_eq!(document["info"]["name"], serde_json::json!("Film"));

        let response = movie_response(&env, "alice", &format!("{}.ts", env.vod_id), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        assert_eq!(body.as_ref(), recording_bytes().as_slice());
    }

    #[tokio::test]
    async fn skip_vod_input_flag_returns_empty_list_for_non_recording_vod_info() {
        let env = env_inner(PlayerRecordingLevel::Read, None, true).await;
        let mut playlist_item =
            create_test_vod_item("http://provider.example/movie/user/pass/77.mp4", "mp4", PlaylistItemType::Video);
        playlist_item.virtual_id = VirtualId::new(77);
        cache_vod_item(&env.app_state, &env.target, playlist_item).await;

        let mut request = api_request(crate::model::XC_ACTION_GET_VOD_INFO);
        request.username = "alice".to_string();
        request.vod_id = "77".to_string();
        let response = xtream_player_api(
            &Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5099".parse().expect("addr")),
            request,
            &env.app_state,
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body, serde_json::json!([]));
    }
}

mod player_api_account_info {
    use super::*;
    use crate::api::endpoints::xtream_api::{xtream_api_register, xtream_player_api};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        response::IntoResponse,
    };
    use shared::model::{
        ApiProxyConfigDto, ApiProxyServerInfoDto, PlayerFeaturesDto, ProxyUserCredentialsDto, TargetUserDto,
        WebAuthConfigDto, WebUiConfigDto,
    };
    use tower::ServiceExt;

    async fn player_api_account_info_response(
        web_ui_enabled: bool,
        web_auth_enabled: bool,
        web_ui_path: Option<&str>,
        user_ui_enabled: bool,
        user_player_api_enabled: Option<bool>,
        global_player_api_enabled: Option<bool>,
    ) -> serde_json::Value {
        let dir = tempdir().expect("temp dir");
        let web_ui_dto = WebUiConfigDto {
            enabled: web_ui_enabled,
            user_ui_enabled: true,
            player_api_enabled: global_player_api_enabled,
            path: web_ui_path.map(ToString::to_string),
            auth: if web_auth_enabled {
                Some(WebAuthConfigDto {
                    enabled: true,
                    issuer: "test".to_string(),
                    secret: "test-secret".to_string(),
                    ..WebAuthConfigDto::default()
                })
            } else {
                None
            },
            ..WebUiConfigDto::default()
        };
        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            user_config_dir: Some(dir.path().join("user_config").to_string_lossy().into_owned()),
            web_ui: Some((&web_ui_dto).into()),
            user_access_control: true,
            ..Config::default()
        };
        let app_state = create_test_app_state(config);
        let api_proxy_dto = ApiProxyConfigDto {
            server: vec![ApiProxyServerInfoDto {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some("8901".to_string()),
                timezone: "UTC".to_string(),
                message: "test".to_string(),
                path: None,
            }],
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: user_ui_enabled,
                    max_connections: 1,
                    player_features: Some(PlayerFeaturesDto {
                        api_enabled: user_player_api_enabled,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let mut api_proxy = crate::model::ApiProxyConfig::from(&api_proxy_dto);
        for target_user in &mut api_proxy.user {
            for credential in &mut target_user.credentials {
                Arc::make_mut(credential).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).expect("set api proxy");
        let target = Arc::new(ConfigTarget { name: "main".to_string(), ..short_epg_target() });
        let sources = SourcesConfig {
            sources: vec![crate::model::ConfigSource { inputs: Vec::new(), targets: vec![Arc::clone(&target)] }],
            ..SourcesConfig::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let request = UserApiRequest {
            username: "alice".to_string(),
            password: "pass".to_string(),
            action: crate::model::XC_ACTION_GET_ACCOUNT_INFO.to_string(),
            ..UserApiRequest::default()
        };
        let fingerprint =
            Fingerprint::new("fp".to_string(), "127.0.0.1".to_string(), "127.0.0.1:5001".parse().expect("addr"));
        let response = xtream_player_api(&fingerprint, request, &app_state).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let text = response_body_text(response).await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    #[tokio::test]
    async fn account_info_player_api_path_with_ui_path() {
        let body = player_api_account_info_response(
            true,       // web_ui_enabled
            true,       // web_auth_enabled
            Some("ui"), // web_ui.path = "ui"
            true,       // user_ui_enabled
            Some(true), // user player_api_enabled
            Some(true), // global player_api_enabled
        )
        .await;
        assert_eq!(body["server_info"]["player_api_path"], "/ui");
    }

    #[tokio::test]
    async fn account_info_player_api_path_empty_path() {
        let body = player_api_account_info_response(
            true,     // web_ui_enabled
            true,     // web_auth_enabled
            Some(""), // empty web_ui.path
            true,     // user_ui_enabled
            Some(true),
            Some(true),
        )
        .await;
        assert_eq!(body["server_info"]["player_api_path"], "/");
    }

    #[tokio::test]
    async fn account_info_player_api_path_omitted_when_player_api_disabled_and_web_ui_disabled() {
        // Case A: User without Player API (api_enabled: false, ui_enabled: false)
        let body_user_disabled = player_api_account_info_response(
            true,
            true,
            Some("ui"),
            false,       // user_ui_enabled: false
            Some(false), // user_player_api_enabled: false
            Some(true),
        )
        .await;
        assert!(body_user_disabled["server_info"].get("player_api_path").is_none());

        // Case B: Web UI disabled
        let body_web_ui_disabled = player_api_account_info_response(
            false, // web_ui_enabled: false
            true,
            Some("ui"),
            true,
            Some(true),
            Some(true),
        )
        .await;
        assert!(body_web_ui_disabled["server_info"].get("player_api_path").is_none());

        // Case C: Web auth disabled
        let body_web_auth_disabled = player_api_account_info_response(
            true,
            false, // web_auth_enabled: false
            Some("ui"),
            true,
            Some(true),
            Some(true),
        )
        .await;
        assert!(body_web_auth_disabled["server_info"].get("player_api_path").is_none());
    }

    #[tokio::test]
    async fn account_info_player_api_path_empty_action_via_router() {
        let dir = tempdir().expect("temp dir");
        let web_ui_dto = WebUiConfigDto {
            enabled: true,
            user_ui_enabled: true,
            player_api_enabled: Some(true),
            path: Some("ui".to_string()),
            auth: Some(WebAuthConfigDto {
                enabled: true,
                issuer: "test".to_string(),
                secret: "test-secret".to_string(),
                ..WebAuthConfigDto::default()
            }),
            ..WebUiConfigDto::default()
        };
        let config = Config {
            storage_dir: dir.path().to_string_lossy().into_owned(),
            user_config_dir: Some(dir.path().join("user_config").to_string_lossy().into_owned()),
            web_ui: Some((&web_ui_dto).into()),
            user_access_control: true,
            ..Config::default()
        };
        let app_state = create_test_app_state(config);
        let api_proxy_dto = ApiProxyConfigDto {
            server: vec![ApiProxyServerInfoDto {
                name: "default".to_string(),
                protocol: "http".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some("8901".to_string()),
                timezone: "UTC".to_string(),
                message: "test".to_string(),
                path: None,
            }],
            user: vec![TargetUserDto {
                target: "main".to_string(),
                credentials: vec![ProxyUserCredentialsDto {
                    username: "alice".to_string(),
                    password: "pass".to_string(),
                    ui_enabled: true,
                    max_connections: 1,
                    player_features: Some(PlayerFeaturesDto { api_enabled: Some(true), ..Default::default() }),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let mut api_proxy = crate::model::ApiProxyConfig::from(&api_proxy_dto);
        for target_user in &mut api_proxy.user {
            for credential in &mut target_user.credentials {
                Arc::make_mut(credential).resolve_plan(&std::collections::HashMap::new());
            }
        }
        app_state.app_config.set_api_proxy(api_proxy).expect("set api proxy");
        let target = Arc::new(ConfigTarget { name: "main".to_string(), ..short_epg_target() });
        let sources = SourcesConfig {
            sources: vec![crate::model::ConfigSource { inputs: Vec::new(), targets: vec![Arc::clone(&target)] }],
            ..SourcesConfig::default()
        };
        app_state.app_config.sources.store(Arc::new(sources));

        let router = xtream_api_register().with_state(Arc::clone(&app_state));
        let mut req = Request::builder()
            .uri("/player_api.php?username=alice&password=pass")
            .body(Body::empty())
            .expect("request builder");
        req.extensions_mut().insert(axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345))));
        let response = router.oneshot(req).await.expect("endpoint response");
        assert_eq!(response.status(), StatusCode::OK);
        let text = response_body_text(response).await.expect("body text");
        let body: serde_json::Value = serde_json::from_str(&text).expect("json body");
        assert_eq!(body["server_info"]["player_api_path"], "/ui");
    }
}

async fn fetch_live_categories(
    state: &Arc<AppState>,
    target: &ConfigTarget,
    category_id: Option<u32>,
    user: &ProxyUserCredentials,
) -> Vec<serde_json::Value> {
    let response = super::xtream_player_api_handle_content_action(
        state,
        target,
        crate::model::XC_ACTION_GET_LIVE_CATEGORIES,
        category_id,
        user,
    )
    .await
    .expect("categories action response")
    .into_response();
    let json: serde_json::Value =
        serde_json::from_str(&response_body_text(response).await.expect("body text")).expect("json");
    json.as_array().expect("categories array").clone()
}

#[tokio::test]
async fn xtream_categories_bouquet_survives_category_absence_and_rejects_foreign_category_id() {
    use crate::repository::{
        persist_playlist, save_user_bouquet, xtream_get_playlist_categories, TargetPlaylistPersistOptions,
    };
    use shared::model::{
        PlaylistBouquetDto, PlaylistClusterBouquetDto, PlaylistGroup, PlaylistItem, PlaylistItemHeader,
    };

    let directory = tempdir().expect("tempdir");
    let state = create_test_app_state(Config {
        storage_dir: directory.path().join("storage").to_string_lossy().into_owned(),
        user_config_dir: Some(directory.path().join("users").to_string_lossy().into_owned()),
        ..Default::default()
    });
    let target = ConfigTarget { use_memory_cache: false, ..short_epg_target() };
    let group = |name: &str, id: u32| {
        let name = name.intern();
        let mut header = PlaylistItemHeader {
            id: id.to_string().intern(),
            name: format!("Channel {name}").intern(),
            title: format!("Channel {name}").intern(),
            group: name.clone(),
            item_type: PlaylistItemType::Live,
            xtream_cluster: XtreamCluster::Live,
            url: format!("http://fixture.invalid/live/{id}").intern(),
            ..Default::default()
        };
        header.gen_uuid();
        PlaylistGroup { id, title: name, channels: vec![PlaylistItem { header }], xtream_cluster: XtreamCluster::Live }
    };

    let opts = TargetPlaylistPersistOptions::default();
    persist_playlist(&state.app_config, &mut [group("A", 1), group("B", 2)], None, &target, None, opts.clone())
        .await
        .expect("initial publish");

    let initial_cats = xtream_get_playlist_categories(&state.app_config, &target.name, XtreamCluster::Live)
        .await
        .expect("initial categories");
    let initial_a_id = initial_cats.iter().find(|c| c.name == "A").expect("category A").id;

    let bouquet = PlaylistBouquetDto {
        xtream: Some(PlaylistClusterBouquetDto { live: Some(vec!["A".to_string()]), ..Default::default() }),
        m3u: None,
    };
    save_user_bouquet(&state.app_config, &target.name, "alice", &bouquet).await.expect("save bouquet");

    persist_playlist(&state.app_config, &mut [group("B", 2)], None, &target, None, opts.clone())
        .await
        .expect("publish without A");
    persist_playlist(&state.app_config, &mut [group("A", 1), group("B", 2)], None, &target, None, opts)
        .await
        .expect("republish with A");

    let republished_cats = xtream_get_playlist_categories(&state.app_config, &target.name, XtreamCluster::Live)
        .await
        .expect("republished categories");
    let new_a_id = republished_cats.iter().find(|c| c.name == "A").expect("category A").id;
    let b_id = republished_cats.iter().find(|c| c.name == "B").expect("category B").id;
    assert_ne!(new_a_id, initial_a_id, "category A must receive a new id after temporary absence");

    let mut user = ProxyUserCredentials::default();
    user.username = "alice".to_string();
    user.output_clusters = ClusterFlags::all();

    let categories = fetch_live_categories(&state, &target, None, &user).await;
    let category_names: Vec<&str> =
        categories.iter().map(|c| c["category_name"].as_str().expect("category_name")).collect();
    assert_eq!(category_names, vec!["A"]);
    assert_eq!(categories[0]["category_id"].as_str(), Some(new_a_id.to_string().as_str()));

    assert_eq!(fetch_live_categories(&state, &target, Some(b_id), &user).await, Vec::<serde_json::Value>::new());
}
