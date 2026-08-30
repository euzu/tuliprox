use super::{
    empty_stream_info_response, get_xtream_player_api_stream_url, is_hls_playback_request, override_live_hls_extension,
    recording_input_matches, resolve_m3u_xtream_timeshift, resolve_xtream_playback_extension, xtream_get_short_epg,
    xtream_player_api_stream, xtream_player_api_stream_with_token, ApiStreamContext, ApiStreamRequest,
    XtreamApiTimeShiftRequest,
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
        &Epg { priority: 0, logo_override: false, attributes: None, children: vec![Arc::new(channel)] },
        &epg_path,
        &std::collections::HashMap::<Arc<str>, Arc<str>>::new(),
        &shared::model::EpgOutputOptions::default(),
    )
    .expect("write target EPG");

    let app_state = create_test_app_state(config);
    let live_item = short_epg_live_item();
    let mut live = BPlusTree::new();
    live.insert(live_item.virtual_id.get(), live_item.clone());
    app_state
        .playlists
        .cache_playlist(
            &target.name,
            PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage {
                live,
                vod: BPlusTree::new(),
                series: BPlusTree::new(),
            })),
        )
        .await;
    let mut id_mapping = BPlusTree::new();
    id_mapping.insert(
        live_item.virtual_id,
        VirtualIdRecord::new(
            live_item.provider_id,
            live_item.virtual_id,
            PlaylistItemType::Live,
            VirtualId::new(0),
            UUIDType::default(),
        ),
    );
    app_state.playlists.cache_id_mapping(&target.name, id_mapping).await;

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
            PlaylistStorage::XtreamPlaylist(Box::new(PlaylistXtreamStorage {
                live,
                vod: BPlusTree::new(),
                series: BPlusTree::new(),
            })),
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
        &Epg { priority: 0, logo_override: false, attributes: None, children: vec![Arc::new(channel)] },
        &epg_path,
        &std::collections::HashMap::<Arc<str>, Arc<str>>::new(),
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
    assert!(!resolved.discriminator.is_empty());
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
