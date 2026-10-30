use super::*;

#[test]
fn disabled_resource_rewrite_proxies_private_xtream_images() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "https://proxy.example".to_string();
    options.flags = XtreamMappingFlagsSet::new();
    options.resource_host_policies.insert("192.168.1.20".to_string(), ResourceOutputPolicy::Proxy);
    options.resource_host_policies.insert("8.8.8.8".to_string(), ResourceOutputPolicy::Direct);
    options.resource_host_policies.insert("127.0.0.1".to_string(), ResourceOutputPolicy::Blocked);

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://192.168.1.20/logo.png",
            "logo",
        ),
        "https://proxy.example/resource/live/user/pass/1/logo"
    );
    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://8.8.8.8/logo.png",
            "logo",
        ),
        "http://8.8.8.8/logo.png"
    );
    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://127.0.0.1/logo.png",
            "logo",
        ),
        ""
    );
    assert_eq!(
        options.get_bd_path_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://192.168.1.20/backdrop.png",
            "",
            0,
        ),
        "https://proxy.example/resource/live/user/pass/1/backdrop_path_0"
    );

    options.resource_proxy_item_types = PlaylistItemTypeSet::empty();
    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(1),
            "http://192.168.1.20/logo.png",
            "logo",
        ),
        "https://proxy.example/resource/live/user/pass/1/logo"
    );
}

#[test]
fn get_resource_url_keeps_internal_paths_for_web_ui_requests() {
    let options = sample_options();

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/abc",
            "logo",
        ),
        "/api/v1/library/thumbnail/abc",
    );
}

#[test]
fn get_bd_path_resource_url_keeps_internal_thumbnail_paths_for_web_ui_requests() {
    let options = sample_options();

    assert_eq!(
        options.get_bd_path_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/backdrop",
            "backdrop_",
            0,
        ),
        "/api/v1/library/thumbnail/backdrop",
    );
}

#[test]
fn get_resource_url_absolutizes_internal_thumbnail_paths_for_xtream_clients() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "http://proxy.example/base".to_string();
    options.reverse_item_types = PlaylistItemTypeSet::empty();
    options.resource_proxy_item_types = PlaylistItemTypeSet::empty();

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::LocalSeries,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/abc",
            "logo",
        ),
        "http://proxy.example/base/api/v1/library/thumbnail/abc",
    );
}

#[test]
fn get_bd_path_resource_url_absolutizes_internal_thumbnail_paths_for_xtream_clients() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "http://proxy.example/base".to_string();
    options.reverse_item_types = PlaylistItemTypeSet::empty();
    options.resource_proxy_item_types = PlaylistItemTypeSet::empty();

    assert_eq!(
        options.get_bd_path_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::LocalSeries,
            VirtualId::new(1),
            "/api/v1/library/thumbnail/backdrop",
            "backdrop_",
            0,
        ),
        "http://proxy.example/base/api/v1/library/thumbnail/backdrop",
    );
}

#[test]
fn get_resource_url_obfuscates_protocol_relative_urls_for_web_ui_requests() {
    let options = sample_options();
    let resource_url = "//cdn.example.com/poster.jpg";

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            resource_url,
            "logo",
        ),
        concat_path(&options.base_url, &seal_web_ui_resource_url(&options.encrypt_secret, resource_url)),
    );
}

#[test]
fn get_resource_url_rewrites_redirect_resources_for_xtream_clients() {
    let mut options = sample_options();
    options.web_ui_request = false;
    options.base_url = "http://proxy.example/iptv".to_string();
    options.reverse_item_types = PlaylistItemTypeSet::empty();
    options.resource_proxy_item_types = PlaylistItemTypeSet::from_item(PlaylistItemType::Live);

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Live,
            PlaylistItemType::Live,
            VirtualId::new(2017),
            "https://provider.example/logo.png",
            "logo",
        ),
        "http://proxy.example/iptv/resource/live/user/pass/2017/logo",
    );
}

#[test]
fn get_resource_url_does_not_bypass_untrusted_root_relative_paths_for_web_ui_requests() {
    let options = sample_options();
    let resource_url = "/provider-controlled/poster.jpg";

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            resource_url,
            "logo",
        ),
        concat_path(&options.base_url, &seal_web_ui_resource_url(&options.encrypt_secret, resource_url)),
    );
}

#[test]
fn get_resource_url_does_not_trust_absolute_urls_containing_internal_thumbnail_path() {
    let options = sample_options();
    let resource_url = "https://provider.example/api/v1/library/thumbnail/abc";

    assert_eq!(
        options.get_resource_url(
            XtreamCluster::Series,
            PlaylistItemType::Series,
            VirtualId::new(1),
            resource_url,
            "logo",
        ),
        concat_path(&options.base_url, &seal_web_ui_resource_url(&options.encrypt_secret, resource_url)),
    );
}

#[test]
fn series_list_projection_skips_details_on_rmp_decode() {
    use crate::model::{
        SeriesListProjection, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
        SeriesStreamDetailSeasonProperties, SeriesStreamProperties,
    };

    let details = SeriesStreamDetailProperties {
        year: Some(2024),
        seasons: Some(vec![SeriesStreamDetailSeasonProperties {
            name: "Season 1".intern(),
            season_number: 1,
            episode_count: 8,
            overview: Some("Overview".intern()),
            air_date: Some("2024-01-01".intern()),
            cover: Some("http://example.test/s1.jpg".intern()),
            cover_tmdb: None,
            cover_big: None,
            duration: None,
        }]),
        episodes: Some(vec![SeriesStreamDetailEpisodeProperties {
            id: 101,
            episode_num: 1,
            season: 1,
            title: "Pilot".intern(),
            container_extension: "mkv".intern(),
            custom_sid: None,
            added: "1700000000".intern(),
            direct_source: "".intern(),
            tmdb: Some(1234),
            release_date: "2024-01-01".intern(),
            series_release_date: None,
            plot: Some("Plot".intern()),
            crew: None,
            duration_secs: 3600,
            duration: "60".intern(),
            movie_image: "http://example.test/e1.jpg".intern(),
            bitrate: 4000,
            rating: Some(8.5),
            video: None,
            audio: None,
        }]),
    };
    let series_props = SeriesStreamProperties {
        name: "Test Series".intern(),
        category_id: 10,
        series_id: 42,
        cast: "Cast".intern(),
        cover: "http://example.test/cover.jpg".intern(),
        director: "Director".intern(),
        genre: Some("Drama".intern()),
        plot: Some("Series Plot".intern()),
        rating: 9.0,
        details: Some(details),
        ..Default::default()
    };
    let item = XtreamPlaylistItem {
        virtual_id: VirtualId::new(42),
        provider_id: 42,
        name: "Test Series".intern(),
        logo: "http://example.test/logo.jpg".intern(),
        logo_small: "".intern(),
        group: "Drama".intern(),
        title: "Test Series".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://example.test/stream".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: Some(StreamProperties::Series(Box::new(series_props))),
        item_type: PlaylistItemType::SeriesInfo,
        category_id: 10,
        input_name: "test-input".intern(),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "42".intern(),
        upstream_user_agent: None,
    };

    let encoded = rmp_serde::to_vec(&item).expect("rmp encode succeeds");

    // Standard decode preserves details
    let decoded_full: XtreamPlaylistItem = rmp_serde::from_slice(&encoded).expect("rmp decode succeeds");
    assert!(decoded_full.has_details());
    let full_series = match decoded_full.additional_properties.as_ref() {
        Some(StreamProperties::Series(s)) => s,
        _ => panic!("expected series properties"),
    };
    assert!(full_series.details.is_some());

    // Projection decode drops details
    let projected: SeriesListProjection = rmp_serde::from_slice(&encoded).expect("projection decode succeeds");
    let decoded_projected = projected.0;
    assert!(!decoded_projected.has_details());
    let projected_series = match decoded_projected.additional_properties.as_ref() {
        Some(StreamProperties::Series(s)) => s,
        _ => panic!("expected series properties"),
    };
    assert!(projected_series.details.is_none());

    // All other series fields are equal
    let mut expected_series = full_series.clone();
    expected_series.details = None;
    assert_eq!(projected_series, &expected_series);

    // Header fields are equal
    assert_eq!(decoded_projected.virtual_id, decoded_full.virtual_id);
    assert_eq!(decoded_projected.provider_id, decoded_full.provider_id);
    assert_eq!(decoded_projected.name, decoded_full.name);
    assert_eq!(decoded_projected.logo, decoded_full.logo);
    assert_eq!(decoded_projected.group, decoded_full.group);
    assert_eq!(decoded_projected.title, decoded_full.title);
    assert_eq!(decoded_projected.url, decoded_full.url);
    assert_eq!(decoded_projected.item_type, decoded_full.item_type);
    assert_eq!(decoded_projected.category_id, decoded_full.category_id);
    assert_eq!(decoded_projected.input_name, decoded_full.input_name);
    assert_eq!(decoded_projected.channel_no, decoded_full.channel_no);
    assert_eq!(decoded_projected.input_stream_id, decoded_full.input_stream_id);
}

#[test]
fn rendered_get_series_json_contains_no_seasons_or_episodes() {
    use crate::model::{
        PlaylistItemTypeSet, SeriesStreamDetailEpisodeProperties, SeriesStreamDetailProperties,
        SeriesStreamDetailSeasonProperties, SeriesStreamProperties, XtreamMappingFlags, XtreamMappingOptions,
    };

    let details = SeriesStreamDetailProperties {
        year: Some(2024),
        seasons: Some(vec![SeriesStreamDetailSeasonProperties {
            name: "Season 1".intern(),
            season_number: 1,
            episode_count: 8,
            overview: Some("Overview".intern()),
            air_date: Some("2024-01-01".intern()),
            cover: Some("http://example.test/s1.jpg".intern()),
            cover_tmdb: None,
            cover_big: None,
            duration: None,
        }]),
        episodes: Some(vec![SeriesStreamDetailEpisodeProperties {
            id: 101,
            episode_num: 1,
            season: 1,
            title: "Pilot".intern(),
            container_extension: "mkv".intern(),
            custom_sid: None,
            added: "1700000000".intern(),
            direct_source: "".intern(),
            tmdb: Some(1234),
            release_date: "2024-01-01".intern(),
            series_release_date: None,
            plot: Some("Plot".intern()),
            crew: None,
            duration_secs: 3600,
            duration: "60".intern(),
            movie_image: "http://example.test/e1.jpg".intern(),
            bitrate: 4000,
            rating: Some(8.5),
            video: None,
            audio: None,
        }]),
    };
    let series_props = SeriesStreamProperties {
        name: "Test Series".intern(),
        category_id: 10,
        series_id: 42,
        cast: "Cast".intern(),
        cover: "http://example.test/cover.jpg".intern(),
        director: "Director".intern(),
        genre: Some("Drama".intern()),
        plot: Some("Series Plot".intern()),
        rating: 9.0,
        details: Some(details),
        ..Default::default()
    };
    let item = XtreamPlaylistItem {
        virtual_id: VirtualId::new(42),
        provider_id: 42,
        name: "Test Series".intern(),
        logo: "http://example.test/logo.jpg".intern(),
        logo_small: "".intern(),
        group: "Drama".intern(),
        title: "Test Series".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "http://example.test/stream".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: Some(StreamProperties::Series(Box::new(series_props))),
        item_type: PlaylistItemType::SeriesInfo,
        category_id: 10,
        input_name: "test-input".intern(),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "42".intern(),
        upstream_user_agent: None,
    };

    let options = XtreamMappingOptions {
        flags: XtreamMappingFlags::RewriteResourceUrl.into(),
        force_redirect: None,
        reverse_item_types: PlaylistItemTypeSet::empty(),
        resource_proxy_item_types: PlaylistItemTypeSet::empty(),
        username: "user".to_string(),
        password: "pass".to_string(),
        base_url: "http://proxy.example".to_string(),
        web_ui_request: false,
        encrypt_secret: [0u8; 16],
        resource_host_policies: Arc::default(),
    };

    let doc = item.to_document(&options);
    let json = serde_json::to_string(&doc).expect("serialize get_series document");
    let value: serde_json::Value = serde_json::from_str(&json).expect("parse json");

    assert!(!json.contains("\"seasons\""), "rendered get_series JSON must not contain seasons");
    assert!(!json.contains("\"episodes\""), "rendered get_series JSON must not contain episodes");
    let obj = value.as_object().expect("expected json object");
    assert!(!obj.contains_key("seasons"));
    assert!(!obj.contains_key("episodes"));
    assert_eq!(obj.get("name").and_then(serde_json::Value::as_str), Some("Test Series"));
    assert_eq!(obj.get("series_id").and_then(serde_json::Value::as_u64), Some(42));
}

#[test]
fn clear_series_details_removes_details() {
    use crate::model::{SeriesStreamDetailProperties, SeriesStreamProperties};

    let details = SeriesStreamDetailProperties { year: Some(2024), seasons: None, episodes: None };
    let series_props =
        SeriesStreamProperties { name: "Test Series".intern(), details: Some(details), ..Default::default() };
    let mut item = XtreamPlaylistItem {
        virtual_id: VirtualId::new(1),
        provider_id: 1,
        name: "Test".intern(),
        logo: "".intern(),
        logo_small: "".intern(),
        group: "".intern(),
        title: "".intern(),
        parent_code: "".intern(),
        rec: "".intern(),
        url: "".intern(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Series,
        additional_properties: Some(StreamProperties::Series(Box::new(series_props))),
        item_type: PlaylistItemType::SeriesInfo,
        category_id: 1,
        input_name: "test".intern(),
        channel_no: 1,
        source_ordinal: 0,
        input_stream_id: "1".intern(),
        upstream_user_agent: None,
    };

    assert!(item.has_details());
    item.clear_series_details();
    assert!(!item.has_details());
    match item.additional_properties {
        Some(StreamProperties::Series(props)) => assert_eq!(props.details, None),
        _ => panic!("expected series properties"),
    }
}
