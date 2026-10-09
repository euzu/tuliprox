use super::{
    access_lease_id_from_variant_uri, cache_test_m3u_hls_item, configure_default_test_server, enable_hls_cache,
    get_response, hls_api_register, m3u_archive_epg_reference_ts, m3u_catchup_epg_reference_from_session_token,
    manifest_media_sequence, path_has_extension, proxy_session_id_from_variant_uri, regression_origin_manifest,
    resolve_leaked_hls_relative_origin, response_body, single_variant_master_playlist, spawn_test_binary_origin,
    store_test_sources_with_target, test_addr, test_app_state, test_app_state_with_inputs, test_fingerprint,
    test_hls_access_context, test_hls_entry_stream_context, test_hls_input, test_hls_share_target, test_m3u_hls_item,
    test_m3u_hls_share_target, TestBinaryOriginResponse, TestSegmentOrigin,
};
use crate::{
    api::model::{
        build_proxy_session_id, AppState, ConnectionKind, HlsAccessLeaseId, HlsOriginSource, HlsOriginSourceKind,
        ProxySessionId,
    },
    model::{ConfigInput, ProxyUserCredentials, TargetUser},
};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, HeaderMap, Request, Response, StatusCode},
    response::IntoResponse,
};
use shared::model::{InputType, PlaylistItemType, UserConnectionPermission, VirtualId, XtreamCluster};
use std::sync::Arc;
use tower::ServiceExt;

#[test]
fn archive_epg_reference_supports_query_and_path_formats() {
    assert_eq!(
        m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?utc=1700000000&lutc=1700003600"),
        Some(1_700_000_000)
    );
    assert_eq!(
        m3u_archive_epg_reference_ts("http://provider/live/archive-1700003600-1700007200.m3u8"),
        Some(1_700_003_600)
    );
    assert_eq!(m3u_archive_epg_reference_ts("http://provider/live/timeshift_abs-1700007200.ts"), Some(1_700_007_200));
    assert_eq!(
        m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000&end=1700003600"),
        Some(1_700_000_000)
    );
}

#[test]
fn archive_epg_reference_rejects_plain_start_queries() {
    assert_eq!(m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?start=1700000000"), None);
}

#[test]
fn date_tree_path_recovers_bittv_archive_epg_reference() {
    assert_eq!(super::super::epg_reference_ts_from_date_tree_path("2026/07/24/14/13/38-06800.ts"), Some(1_784_902_418));
    assert_eq!(
        super::super::epg_reference_ts_from_date_tree_path("dvr-2026/07/24/14/13/38-06800.ts"),
        super::super::epg_reference_ts_from_date_tree_path("2026/07/24/14/13/38-06800.ts")
    );
    assert!(super::super::looks_like_archive_media_path("2026/07/24/14/13/38-06800.ts"));
}

#[test]
fn archive_media_path_does_not_accept_plain_202_prefixed_segments() {
    assert!(!super::super::looks_like_archive_media_path("2026.ts"));
    assert!(!super::super::looks_like_archive_media_path("202_media.ts"));
}

#[test]
fn session_token_recovers_archive_epg_reference_when_media_url_lost_markers() {
    assert_eq!(
        m3u_catchup_epg_reference_from_session_token("m3u-catchup|user|42|archive|1717200000|3600"),
        Some(1_717_200_000)
    );
    assert_eq!(m3u_catchup_epg_reference_from_session_token("m3u-catchup|user|42|live"), None);
}

#[test]
fn append_catchup_session_hint_keeps_m3u_catchup_token_without_shared_hls_cache() {
    let fingerprint = test_fingerprint();
    let hint = "m3u-catchup|fp|alice|42|deadbeef";
    let token = super::super::hls_entry_user_session_token(&fingerprint, "alice", 42, Some(hint), Some(1_717_200_000));
    assert_eq!(token, hint);
    assert!(super::super::is_m3u_catchup_session_token(&token));

    let from_archive = super::super::hls_entry_user_session_token(&fingerprint, "alice", 42, None, Some(1_717_200_000));
    assert!(from_archive.contains("|archive|1717200000|0"));
    assert!(super::super::is_m3u_catchup_session_token(&from_archive));
    assert!(super::super::is_m3u_catchup_session_token("m3u-catchup|fp|alice|42|timeshift_abs|1717200000|0"));
}

#[test]
fn leaked_dvr_relative_joins_against_media_playlist_and_dvr_session_root() {
    assert_eq!(
        resolve_leaked_hls_relative_origin(
            "http://cdn.example/big/aa_1/media.m3u8",
            "dvr-2026/07/26/15/30/59-06000.ts",
            Some("token=abc"),
        ),
        Some("http://cdn.example/big/aa_1/dvr-2026/07/26/15/30/59-06000.ts?token=abc".to_string())
    );
    assert_eq!(
        resolve_leaked_hls_relative_origin(
            "http://cdn.example/big/aa_1/dvr-2026/07/26/15/30/59-06000.ts?token=old",
            "dvr-2026/07/26/15/31/05-06000.ts",
            Some("token=new"),
        ),
        Some("http://cdn.example/big/aa_1/dvr-2026/07/26/15/31/05-06000.ts?token=new".to_string())
    );
    assert_eq!(
        resolve_leaked_hls_relative_origin("http://cdn.example/big/aa_1/media.m3u8", "segment001.ts", None,),
        None
    );
    assert_eq!(
        resolve_leaked_hls_relative_origin(
            "http://cdn.example/big/aa_1/media.m3u8",
            "dvr-2026/07/26/15/30/../59-06000.ts",
            None,
        ),
        None
    );
    assert_eq!(resolve_leaked_hls_relative_origin("http://cdn.example/live.m3u8", "./dvr-2026/a.ts", None), None);
}

#[test]
fn archive_epg_reference_supports_contextual_start_aliases() {
    assert_eq!(
        m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?offset=-3600&utcstart=1717200000"),
        Some(1_717_200_000)
    );
    assert_eq!(
        m3u_archive_epg_reference_ts("http://provider/live/42.m3u8?timestamp=1717200000&offset=120"),
        Some(1_717_200_000)
    );
}

#[test]
fn hls_cache_session_tokens_separate_live_and_archive_playback() {
    let fingerprint = test_fingerprint();
    let live = super::super::create_hls_cache_user_session_token(&fingerprint, "user", 31, None, None);
    let archive =
        super::super::create_hls_cache_user_session_token(&fingerprint, "user", 31, None, Some(1_784_898_000));

    assert!(!super::super::is_m3u_catchup_session_token(&live));
    assert!(super::super::is_m3u_catchup_session_token(&archive));
    assert_ne!(live, archive);
}

#[test]
fn hls_cache_session_token_preserves_existing_m3u_catchup_identity() {
    let fingerprint = test_fingerprint();
    let existing = "m3u-catchup|fp|user|31|archive|1784898000|3600";
    let token = super::super::create_hls_cache_user_session_token(
        &fingerprint,
        "user",
        31,
        Some(existing),
        Some(1_784_898_000),
    );

    assert!(token.starts_with(existing));
    assert!(token.contains("|hls-cache|"));
}

#[tokio::test]
async fn hls_cache_stream_channel_uses_archive_epg_context() {
    let app_state = test_app_state();
    let mut access = test_hls_access_context(
        ProxySessionId("proxy-archive".to_string()),
        HlsAccessLeaseId("lease-archive".to_string()),
    );
    access.epg_reference_ts = Some(1_784_898_000);
    access.archive_origin_url = Some("http://provider/channel/timeshift_abs-1784898000.m3u8".to_string());
    let origin_source =
        HlsOriginSource::new(1, Arc::from("test-input"), "80510", HlsOriginSourceKind::M3uMediaPlaylist)
            .with_archive_reference(1_784_898_000);

    let channel = super::super::build_hls_cache_stream_channel(
        &app_state,
        &access,
        &origin_source,
        &ProxySessionId("proxy-archive".to_string()),
    )
    .await;

    assert_eq!(channel.item_type, PlaylistItemType::Catchup);
    assert_eq!(channel.cluster, XtreamCluster::Video);
    assert_eq!(channel.epg_reference_ts, Some(1_784_898_000));
}

#[tokio::test]
async fn hls_cache_manifest_context_restores_leased_archive_origin() -> Result<(), StatusCode> {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    let input = ConfigInput {
        id: 1,
        name: Arc::from("test-input"),
        input_type: InputType::M3u,
        enabled: true,
        ..ConfigInput::default()
    };
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_string();
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    cache_test_m3u_hls_item(
        &app_state,
        &target,
        test_m3u_hls_item(&input, 12345, "80510", "http://provider/channel/mono.m3u8"),
    )
    .await;

    let archive_url = "http://provider/channel/timeshift_abs-1784898000.m3u8";
    let mut access = test_hls_access_context(
        ProxySessionId("proxy-archive".to_string()),
        HlsAccessLeaseId("lease-archive".to_string()),
    );
    access.stream_ref = "80510".to_string();
    access.epg_reference_ts = Some(1_784_898_000);
    access.archive_origin_url = Some(archive_url.to_string());

    let context =
        super::super::resolve_hls_playback_manifest_request_context(&app_state, &access, &HeaderMap::new()).await?;

    assert_eq!(context.hls_url, archive_url);
    assert_eq!(context.origin_source.stream_ref, "80510");
    assert_eq!(context.origin_source.archive_reference, Some(1_784_898_000));
    Ok(())
}

#[tokio::test]
async fn hls_cache_archive_entry_uses_distinct_identity_and_preserves_origin() -> Result<(), &'static str> {
    let app_state = test_app_state();
    enable_hls_cache(&app_state);
    configure_default_test_server(&app_state);
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_string();
    user.password = "hls-pass".to_string();
    let input = test_hls_input();
    let target = test_hls_share_target(true);
    let archive_url = "http://origin.example.com/live/user/pass/timeshift_abs-1784898000.m3u8";

    let response = super::super::handle_hls_stream_request(
        &test_fingerprint(),
        &app_state,
        &user,
        &target,
        None,
        None,
        archive_url,
        Some(1_784_898_000),
        test_hls_entry_stream_context(12345, "80510", None),
        &input,
        &HeaderMap::new(),
        UserConnectionPermission::Allowed,
        Some(ConnectionKind::Normal),
        &super::super::build_virtual_hls_entry_path(&target, &input, &user, 12345),
        super::super::HlsRequestStage::Entry,
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::OK);
    let (_, media_playlist_uri) = single_variant_master_playlist(response).await;
    let proxy_session_id = ProxySessionId(proxy_session_id_from_variant_uri(&media_playlist_uri).to_string());
    let live_source = super::super::build_hls_origin_source(&input, "80510");
    let archive_source =
        super::super::build_hls_origin_source_for_playback(&input, "80510", Some(1_784_898_000), Some(archive_url));
    assert_ne!(proxy_session_id, build_proxy_session_id(&live_source.session_key(), &app_state.get_encrypt_secret()));
    assert_eq!(
        proxy_session_id,
        build_proxy_session_id(&archive_source.session_key(), &app_state.get_encrypt_secret())
    );

    let access_lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&media_playlist_uri).to_string());
    let lease = app_state
        .hls
        .proxy
        .access_leases()
        .write()
        .await
        .response_snapshot(&access_lease_id, &proxy_session_id, super::super::current_time_millis())
        .ok_or("archive access lease")?;
    assert_eq!(lease.stream_ref, "80510");
    assert_eq!(lease.epg_reference_ts, Some(1_784_898_000));
    assert_eq!(lease.archive_origin_url.as_deref(), Some(archive_url));
    assert!(super::super::is_m3u_catchup_session_token(&lease.user_session_token));
    Ok(())
}

pub(in crate::api::endpoints::hls_api::tests) struct BoundedFlussonicFixture {
    pub(in crate::api::endpoints::hls_api::tests) _temp: tempfile::TempDir,
    pub(in crate::api::endpoints::hls_api::tests) origin: TestSegmentOrigin,
    pub(in crate::api::endpoints::hls_api::tests) app_state: Arc<AppState>,
    pub(in crate::api::endpoints::hls_api::tests) requests: Arc<std::sync::Mutex<Vec<String>>>,
    pub(in crate::api::endpoints::hls_api::tests) catchup_template: String,
    pub(in crate::api::endpoints::hls_api::tests) live_uri: String,
}

pub(in crate::api::endpoints::hls_api::tests) async fn bounded_flussonic_request(
    app_state: Arc<AppState>,
    url: &str,
) -> Result<Response<Body>, Box<dyn std::error::Error>> {
    let parsed = url::Url::parse(url)?;
    let mut uri = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        uri.push('?');
        uri.push_str(query);
    }
    let router = crate::api::endpoints::m3u_api::m3u_api_register().merge(hls_api_register()).with_state(app_state);
    let mut request = Request::builder().uri(uri).body(Body::empty())?;
    request.extensions_mut().insert(ConnectInfo(test_addr()));
    Ok(router.oneshot(request).await?)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::api::endpoints::hls_api::tests) enum BoundedArchiveOrigin {
    Media,
    Missing,
    SingleVariantMaster,
}

pub(in crate::api::endpoints::hls_api::tests) async fn bounded_flussonic_origin(
    archive_origin: BoundedArchiveOrigin,
    requests: &Arc<std::sync::Mutex<Vec<String>>>,
) -> Result<TestSegmentOrigin, Box<dyn std::error::Error>> {
    let recorded = Arc::clone(requests);
    let base = Arc::new(std::sync::OnceLock::<String>::new());
    let origin_base = Arc::clone(&base);
    let segment = Arc::<[u8]>::from(
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test/fixtures/hls/channel_unavailable.ts"))
            .as_slice(),
    );
    let origin = spawn_test_binary_origin(Arc::new(move |path| {
        if let Ok(mut paths) = recorded.lock() {
            paths.push(path.to_owned());
        }
        let resource = path.split('?').next().unwrap_or(path);
        if resource == "/input.m3u" {
            let Some(host) = origin_base.get() else {
                return TestBinaryOriginResponse::new(StatusCode::INTERNAL_SERVER_ERROR, Arc::from(&b"origin"[..]));
            };
            let text = format!(
                "#EXTM3U\n#EXTINF:-1 catchup=\"fs\" catchup-days=\"7\",Channel\n{host}/channel/mono.m3u8?token=a%2Fb\n"
            );
            return TestBinaryOriginResponse::new(StatusCode::OK, Arc::from(text.into_bytes()));
        }
        if std::path::Path::new(resource).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
            && ["/channel/archive-", "/channel/live/", "/channel/tracks-v1/archive-"]
                .iter()
                .any(|prefix| resource.starts_with(prefix))
        {
            return TestBinaryOriginResponse::new(StatusCode::OK, Arc::clone(&segment));
        }
        let archive_file = resource
            .strip_prefix("/channel/")
            .and_then(|file| file.strip_suffix(".m3u8"))
            .filter(|file| file.starts_with("archive-"));
        if let (BoundedArchiveOrigin::SingleVariantMaster, Some(archive)) = (archive_origin, archive_file) {
            let master = format!(
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=9150000,RESOLUTION=1920x1080\ntracks-v1/{archive}.ts.m3u8\n"
            );
            return TestBinaryOriginResponse::new(StatusCode::OK, Arc::from(master.into_bytes()));
        }
        let media_directory = if resource == "/channel/mono.m3u8" {
            Some("live")
        } else if archive_origin == BoundedArchiveOrigin::SingleVariantMaster {
            resource
                .strip_prefix("/channel/tracks-v1/")
                .and_then(|file| file.strip_suffix(".ts.m3u8"))
                .filter(|file| file.starts_with("archive-"))
        } else {
            archive_file
        };
        if let Some(directory) = media_directory {
            if archive_origin == BoundedArchiveOrigin::Missing && directory != "live" {
                return TestBinaryOriginResponse::new(StatusCode::NOT_FOUND, Arc::from(&b"unavailable"[..]));
            }
            let mut manifest = String::from_utf8_lossy(&regression_origin_manifest(123, 6)).into_owned();
            for sequence in 123..129 {
                manifest =
                    manifest.replace(&format!("{sequence}.ts"), &format!("{directory}/{sequence}.ts?token=a%2Fb"));
            }
            if directory != "live" {
                manifest.push_str("#EXT-X-ENDLIST\n");
            }
            return TestBinaryOriginResponse::new(StatusCode::OK, Arc::from(manifest.into_bytes()));
        }
        TestBinaryOriginResponse::new(StatusCode::NOT_FOUND, Arc::from(&b"unexpected origin path"[..]))
    }))
    .await;
    base.set(origin.base_url.clone()).map_err(|_| "origin already configured")?;
    Ok(origin)
}

pub(in crate::api::endpoints::hls_api::tests) async fn bounded_flussonic_hls_fixture(
    archive_origin: BoundedArchiveOrigin,
) -> Result<BoundedFlussonicFixture, Box<dyn std::error::Error>> {
    use shared::model::{ConfigInputOptionsDto, FlussonicHlsCatchup, ProxyType};
    use tuliprox_core::model::ConfigInputOptions;
    use tuliprox_repository::{ensure_target_storage_path, m3u_write_playlist};

    let temp = tempfile::tempdir()?;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let origin = bounded_flussonic_origin(archive_origin, &requests).await?;
    let options: ConfigInputOptionsDto = serde_json::from_str(r#"{"flussonic_hls_catchup":"bounded_archive"}"#)?;
    assert_eq!(options.flussonic_hls_catchup, FlussonicHlsCatchup::BoundedArchive);
    let input = ConfigInput {
        id: 1,
        name: Arc::from("bounded-flussonic"),
        input_type: InputType::M3u,
        url: format!("{}/input.m3u", origin.base_url),
        enabled: true,
        options: Some(ConfigInputOptions::from(&options)),
        ..ConfigInput::default()
    };
    let mut target = test_m3u_hls_share_target();
    target.name = "default".to_owned();
    let app_state = test_app_state_with_inputs(vec![Arc::new(input.clone())]);
    enable_hls_cache(&app_state);
    let mut config = (*app_state.app_config.config.load_full()).clone();
    config.storage_dir = temp.path().to_string_lossy().into_owned();
    config.custom_stream_response_enabled = false;
    app_state.app_config.config.store(Arc::new(config));
    configure_default_test_server(&app_state);
    store_test_sources_with_target(&app_state, input.clone(), target.clone());
    let mut user = ProxyUserCredentials::default();
    user.username = "hls-user".to_owned();
    user.password = "hls-pass".to_owned();
    user.proxy = ProxyType::Reverse(None);
    let mut proxy = app_state.app_config.api_proxy.load_full().ok_or("api proxy")?.as_ref().clone();
    proxy.user = vec![TargetUser { target: target.name.clone(), credentials: vec![Arc::new(user)] }];
    app_state.app_config.api_proxy.store(Some(Arc::new(proxy)));
    let (mut groups, errors) = crate::iptv::m3u::download_m3u_playlist(
        &app_state.app_config,
        &reqwest::Client::new(),
        &app_state.app_config.config.load_full(),
        &input,
    )
    .await;
    assert_eq!(errors.len(), 0, "import errors: {errors:?}");
    let item = groups.first_mut().and_then(|group| group.channels.first_mut()).ok_or("imported item")?;
    item.header.virtual_id = VirtualId::new(12345);
    assert_eq!(item.header.url.as_ref(), format!("{}/channel/mono.m3u8?token=a%2Fb", origin.base_url));
    let target_path = ensure_target_storage_path(&app_state.app_config.config.load(), &target.name).await?;
    m3u_write_playlist(
        &app_state.app_config,
        &target,
        target.get_m3u_output().ok_or("M3U output")?,
        &target_path,
        &groups,
        false,
    )
    .await?;
    let response = bounded_flussonic_request(
        Arc::clone(&app_state),
        "http://proxy/get.php?username=hls-user&password=hls-pass&type=m3u_plus",
    )
    .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let export = String::from_utf8(response_body(response).await.to_vec())?;
    let catchup_template = export
        .split("catchup-source=\"")
        .nth(1)
        .and_then(|tail| tail.split('"').next())
        .ok_or("exported catchup source")?
        .to_owned();
    assert!(export.contains("catchup=\"default\""));
    assert!(!export.contains("a%2Fb"));
    let live_uri =
        export.lines().find(|line| !line.starts_with('#') && !line.is_empty()).ok_or("exported live URL")?.to_owned();
    Ok(BoundedFlussonicFixture { _temp: temp, origin, app_state, requests, catchup_template, live_uri })
}

pub(in crate::api::endpoints::hls_api::tests) async fn bounded_flussonic_hls_entry(
    fixture: &BoundedFlussonicFixture,
    start: i64,
    duration: i64,
) -> Result<Response<Body>, Box<dyn std::error::Error>> {
    let url =
        fixture.catchup_template.replace("{utc}", &start.to_string()).replace("{duration}", &duration.to_string());
    bounded_flussonic_request(Arc::clone(&fixture.app_state), &url).await
}

#[tokio::test]
async fn bounded_flussonic_hls_preserves_master_child_segments_and_duration_identity(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = bounded_flussonic_hls_fixture(BoundedArchiveOrigin::Media).await?;
    let mut identities = std::collections::HashSet::new();
    for (start, duration) in [(1_784_898_000, 3600), (1_784_898_000, 7200), (1_784_898_600, 3600)] {
        let entry = bounded_flussonic_hls_entry(&fixture, start, duration).await?;
        assert_eq!(entry.status(), StatusCode::OK);
        let (_, uri) = single_variant_master_playlist(entry).await;
        let proxy_id = ProxySessionId(proxy_session_id_from_variant_uri(&uri).to_owned());
        assert!(identities.insert(proxy_id.clone()));
        let lease_id = HlsAccessLeaseId(access_lease_id_from_variant_uri(&uri).to_owned());
        let lease = fixture
            .app_state
            .hls
            .proxy
            .access_lease_response_snapshot(&lease_id, &proxy_id, super::super::current_time_millis())
            .await
            .ok_or("lease")?;
        let archive = format!("archive-{start}-{duration}");
        let expected_origin = format!("{}/channel/{archive}.m3u8?token=a%2Fb", fixture.origin.base_url);
        assert_eq!(lease.archive_origin_url.as_deref(), Some(expected_origin.as_str()));
        assert!(lease.user_session_token.contains(&format!("|archive|{start}|{duration}|hls-cache|")));
        let media = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(media.status(), StatusCode::OK);
        let body = String::from_utf8(response_body(media).await.to_vec())?;
        let segment_uri =
            body.lines().find(|line| line.starts_with("/hls/") && path_has_extension(line, "ts")).ok_or("segment")?;
        let segment = get_response(Arc::clone(&fixture.app_state), segment_uri, None).await;
        assert_eq!(segment.status(), StatusCode::OK);
        assert_ne!(response_body(segment).await, bytes::Bytes::new());
        let reload = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
        assert_eq!(reload.status(), StatusCode::OK);
        let reload_body = String::from_utf8(response_body(reload).await.to_vec())?;
        assert_eq!(manifest_media_sequence(&body), manifest_media_sequence(&reload_body));
        let paths = fixture.requests.lock().map_err(|_| "request log")?;
        assert!(paths.iter().any(|p| p == &format!("/channel/{archive}.m3u8?token=a%2Fb")));
        assert!(paths.iter().any(|p| p.starts_with(&format!("/channel/{archive}/")) && p.ends_with(".ts?token=a%2Fb")));
        assert!(!paths.iter().any(|p| p.contains("timeshift_abs") || p.contains("mono.m3u8")));
    }
    Ok(())
}

#[tokio::test]
async fn bounded_flussonic_hls_resolves_single_variant_archive_master() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = bounded_flussonic_hls_fixture(BoundedArchiveOrigin::SingleVariantMaster).await?;
    let entry = bounded_flussonic_hls_entry(&fixture, 1_784_898_000, 3600).await?;
    assert_eq!(entry.status(), StatusCode::OK);
    let (_, uri) = single_variant_master_playlist(entry).await;
    let media = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(media.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(media).await.to_vec())?;
    let segment_uri =
        body.lines().find(|line| line.starts_with("/hls/") && path_has_extension(line, "ts")).ok_or("segment")?;
    let segment = get_response(Arc::clone(&fixture.app_state), segment_uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert_ne!(response_body(segment).await, bytes::Bytes::new());
    let reload = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(reload.status(), StatusCode::OK);
    let reload_body = String::from_utf8(response_body(reload).await.to_vec())?;
    assert_eq!(manifest_media_sequence(&body), manifest_media_sequence(&reload_body));
    let paths = fixture.requests.lock().map_err(|_| "request log")?;
    assert!(paths.iter().any(|p| p == "/channel/archive-1784898000-3600.m3u8?token=a%2Fb"));
    assert!(paths.iter().any(|p| p == "/channel/tracks-v1/archive-1784898000-3600.ts.m3u8?token=a%2Fb"));
    assert!(paths
        .iter()
        .any(|p| p.starts_with("/channel/tracks-v1/archive-1784898000-3600/") && p.ends_with(".ts?token=a%2Fb")));
    assert!(!paths.iter().any(|p| p.contains("timeshift_abs") || p.contains("mono.m3u8")));
    Ok(())
}

#[tokio::test]
async fn bounded_flussonic_hls_404_is_unavailable_without_live_or_timeshift_fallback(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = bounded_flussonic_hls_fixture(BoundedArchiveOrigin::Missing).await?;
    let entry = bounded_flussonic_hls_entry(&fixture, 1_784_898_000, 3600).await?;
    assert_eq!(entry.status(), StatusCode::OK);
    let (_, uri) = single_variant_master_playlist(entry).await;
    let response = get_response(Arc::clone(&fixture.app_state), &uri, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key(header::LOCATION));
    {
        let paths = fixture.requests.lock().map_err(|_| "request log")?;
        assert!(paths.iter().any(|p| p == "/channel/archive-1784898000-3600.m3u8?token=a%2Fb"));
        assert!(!paths.iter().any(|p| p.contains("timeshift_abs") || p.contains("mono.m3u8")));
    }
    let live = bounded_flussonic_request(Arc::clone(&fixture.app_state), &fixture.live_uri).await?;
    assert_eq!(live.status(), StatusCode::OK);
    let (_, live_manifest) = single_variant_master_playlist(live).await;
    let media = get_response(Arc::clone(&fixture.app_state), &live_manifest, None).await;
    assert_eq!(media.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(media).await.to_vec())?;
    let segment_uri =
        body.lines().find(|line| line.starts_with("/hls/") && path_has_extension(line, "ts")).ok_or("live segment")?;
    let segment = get_response(Arc::clone(&fixture.app_state), segment_uri, None).await;
    assert_eq!(segment.status(), StatusCode::OK);
    assert_ne!(response_body(segment).await, bytes::Bytes::new());
    let paths = fixture.requests.lock().map_err(|_| "request log")?;
    assert!(paths.iter().any(|p| p == "/channel/mono.m3u8?token=a%2Fb"));
    assert!(paths.iter().any(|p| p.starts_with("/channel/live/") && p.ends_with(".ts?token=a%2Fb")));
    assert!(!paths.iter().any(|p| p.contains("timeshift_abs")));
    Ok(())
}

#[test]
fn cache_enabled_legacy_hls_route_only_allows_existing_m3u_catchup_session() {
    assert!(super::super::legacy_hls_route_allowed_with_cache(
        true,
        Some("m3u-catchup|session"),
        Some("m3u-catchup|session")
    ));
    assert!(super::super::legacy_hls_route_allowed_with_cache(true, Some("catchup|session"), Some("catchup|session")));
    assert!(!super::super::legacy_hls_route_allowed_with_cache(
        true,
        Some("m3u-catchup|session"),
        Some("m3u-catchup|other")
    ));
    assert!(!super::super::legacy_hls_route_allowed_with_cache(true, Some("legacy-session"), Some("legacy-session")));
    assert!(!super::super::legacy_hls_route_allowed_with_cache(true, None, None));
    assert!(super::super::legacy_hls_route_allowed_with_cache(false, None, None));
}
