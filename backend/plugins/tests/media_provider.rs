//! Media catalog provider integration tests (W-A1 slice).
//!
//! Validates:
//! - Golden acceptance test A3: fixture catalog mapped via plugin equals direct core mapper.
//! - Paged catalog fetch across multiple cursor pages.
//! - Evil probe modes: trap, hang past deadline, repeating cursor loop, oversized strings/lists,
//!   duplicate ids, dangling references.
//! - Component exporting both `input-provider` and `media-provider-v1-1` is rejected.
//! - Manifest hook `media.fetch` validation and interface binding.
//! - `MediaServerStreamRef::Plugin` and `MediaServerImageRef::Plugin` internal URL round trips.
//! - v1.0 `input-provider` continues to function unchanged.

mod common;

use common::{load_probe, probe_entry};
use shared::model::{
    HookCtx, MediaServerLibraryKind, PlaylistGroup, PlaylistItemType, PluginHost, PluginImageRequest,
    PluginInputRequest, PluginStreamRequest, XtreamCluster,
};
use std::sync::Arc;
use tuliprox_media_server::{
    media_server_catalog_snapshot_to_playlist,
    playback::{parse_media_server_image_ref, parse_media_server_stream_ref},
    playlist_mapper::{media_server_image_ref_to_internal_url, media_server_stream_ref_to_internal_url},
    MediaServerCatalogSnapshot, MediaServerDescriptiveFacts, MediaServerEpisode, MediaServerImageRef,
    MediaServerLibrary, MediaServerLibraryRef, MediaServerMovie, MediaServerProviderIdHint, MediaServerSeason,
    MediaServerSeries, MediaServerStreamRef, MediaServerTechnicalFacts, MediaServerVideoTechnicalFacts,
};
use tuliprox_plugins::check_conflicting_exports;

fn assert_groups_equal(a: &[PlaylistGroup], b: &[PlaylistGroup]) {
    assert_eq!(a.len(), b.len(), "group count matches");
    for (ga, gb) in a.iter().zip(b.iter()) {
        assert_eq!(ga.id, gb.id, "group id matches");
        assert_eq!(ga.title, gb.title, "group title matches");
        assert_eq!(ga.xtream_cluster, gb.xtream_cluster, "group cluster matches");
        assert_eq!(ga.channels.len(), gb.channels.len(), "channels count matches");
        for (ca, cb) in ga.channels.iter().zip(gb.channels.iter()) {
            assert_eq!(ca.header.id, cb.header.id, "item id matches");
            assert_eq!(ca.header.name, cb.header.name, "item name matches");
            assert_eq!(ca.header.title, cb.header.title, "item title matches");
            assert_eq!(ca.header.url, cb.header.url, "item url matches");
            assert_eq!(ca.header.item_type, cb.header.item_type, "item type matches");
            assert_eq!(ca.header.xtream_cluster, cb.header.xtream_cluster, "item cluster matches");
            assert_eq!(ca.header.group, cb.header.group, "item group matches");
            assert_eq!(ca.header.logo, cb.header.logo, "item logo matches");
            assert_eq!(ca.header.input_name, cb.header.input_name, "item input_name matches");
            assert_eq!(
                ca.header.additional_properties, cb.header.additional_properties,
                "item additional_properties matches"
            );
        }
    }
}

fn request(options: &[(String, String)]) -> PluginInputRequest<'_> {
    PluginInputRequest {
        input_name: "feed",
        url: "http://host/media",
        username: Some("user"),
        password: Some("pass"),
        options,
        refresh: false,
        last_success_at: None,
        event_sink: None,
        run_id: None,
        execution_order: None,
        input_id: None,
    }
}

fn expected_libraries(input_name: &Arc<str>, server_id: &Arc<str>) -> Vec<MediaServerLibrary> {
    vec![
        MediaServerLibrary {
            reference: MediaServerLibraryRef {
                input_name: Arc::clone(input_name),
                server_id: Arc::clone(server_id),
                library_id: Arc::from("lib-movies"),
            },
            name: Arc::from("Movies"),
            kind: MediaServerLibraryKind::Movies,
        },
        MediaServerLibrary {
            reference: MediaServerLibraryRef {
                input_name: Arc::clone(input_name),
                server_id: Arc::clone(server_id),
                library_id: Arc::from("lib-tv"),
            },
            name: Arc::from("TV Shows"),
            kind: MediaServerLibraryKind::TvShows,
        },
    ]
}

fn expected_movies(input_name: &Arc<str>, server_id: &Arc<str>) -> Vec<MediaServerMovie> {
    vec![MediaServerMovie {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        library_id: Arc::from("lib-movies"),
        item_id: Arc::from("movie-1"),
        title: Arc::from("The Big Movie"),
        year: Some(2023),
        release_date: Some(Arc::from("2023-05-15")),
        source_version_hint: None,
        provider_hints: vec![MediaServerProviderIdHint { namespace: Arc::from("tmdb"), value: Arc::from("1001") }],
        descriptive_facts: Some(MediaServerDescriptiveFacts {
            original_title: Some(Arc::from("The Big Movie")),
            sort_title: Some(Arc::from("Big Movie, The")),
            summary: Some(Arc::from("An epic movie description")),
            tagline: Some(Arc::from("The biggest of all")),
            studio: Some(Arc::from("Studio A")),
            network: None,
            content_rating: Some(Arc::from("PG-13")),
            parental_age: Some(13),
            audience_rating: Some(Arc::from("8.5")),
            genres: vec![Arc::from("Action"), Arc::from("Sci-Fi")],
            countries: vec![Arc::from("US")],
            directors: vec![Arc::from("Director One")],
            writers: vec![Arc::from("Writer One")],
            cast: vec![Arc::from("Actor One"), Arc::from("Actor Two")],
            crew: vec![Arc::from("Producer One")],
        }),
        technical_facts: Some(MediaServerTechnicalFacts {
            container: Some(Arc::from("mp4")),
            duration_secs: Some(7200),
            bitrate: Some(5_000_000),
            video: Some(MediaServerVideoTechnicalFacts {
                codec: Some(Arc::from("h264")),
                width: Some(1920),
                height: Some(1080),
            }),
            audio: None,
        }),
        stream_ref: Some(MediaServerStreamRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(server_id),
            item_id: Arc::from("movie-1"),
            stream_key: Some(Arc::from("stream-movie-1")),
        }),
        image_ref: Some(MediaServerImageRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(server_id),
            item_id: Arc::from("movie-1"),
            image_key: Arc::from("image-movie-1"),
        }),
    }]
}

fn expected_series(input_name: &Arc<str>, server_id: &Arc<str>) -> Vec<MediaServerSeries> {
    vec![MediaServerSeries {
        input_name: Arc::clone(input_name),
        server_id: Arc::clone(server_id),
        library_id: Arc::from("lib-tv"),
        item_id: Arc::from("series-1"),
        title: Arc::from("Epic Series"),
        year: Some(2022),
        release_date: Some(Arc::from("2022-01-10")),
        source_version_hint: None,
        provider_hints: vec![MediaServerProviderIdHint { namespace: Arc::from("tmdb"), value: Arc::from("2001") }],
        descriptive_facts: Some(MediaServerDescriptiveFacts {
            original_title: Some(Arc::from("Epic Series")),
            sort_title: Some(Arc::from("Epic Series")),
            summary: Some(Arc::from("A gripping series drama")),
            tagline: Some(Arc::from("Winter is arriving")),
            studio: Some(Arc::from("Network TV")),
            network: Some(Arc::from("Network TV")),
            content_rating: Some(Arc::from("TV-MA")),
            parental_age: Some(17),
            audience_rating: Some(Arc::from("9.0")),
            genres: vec![Arc::from("Drama")],
            countries: vec![Arc::from("US")],
            directors: vec![Arc::from("Showrunner One")],
            writers: vec![Arc::from("Writer Two")],
            cast: vec![Arc::from("Star Actor")],
            crew: vec![],
        }),
        child_count: Some(2),
        episode_count: Some(3),
        image_ref: Some(MediaServerImageRef::Plugin {
            input_name: Arc::clone(input_name),
            plugin_id: Arc::clone(server_id),
            item_id: Arc::from("series-1"),
            image_key: Arc::from("image-series-1"),
        }),
    }]
}

fn expected_seasons(input_name: &Arc<str>, server_id: &Arc<str>) -> Vec<MediaServerSeason> {
    vec![
        MediaServerSeason {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(server_id),
            library_id: Arc::from("lib-tv"),
            item_id: Arc::from("season-1"),
            series_id: Some(Arc::from("series-1")),
            series_title: Some(Arc::from("Epic Series")),
            title: Arc::from("Season 1"),
            season: Some(1),
            year: Some(2022),
            release_date: Some(Arc::from("2022-01-10")),
            source_version_hint: None,
            provider_hints: vec![],
            descriptive_facts: None,
            episode_count: Some(2),
            image_ref: Some(MediaServerImageRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("season-1"),
                image_key: Arc::from("image-season-1"),
            }),
        },
        MediaServerSeason {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(server_id),
            library_id: Arc::from("lib-tv"),
            item_id: Arc::from("season-2"),
            series_id: Some(Arc::from("series-1")),
            series_title: Some(Arc::from("Epic Series")),
            title: Arc::from("Season 2"),
            season: Some(2),
            year: Some(2023),
            release_date: Some(Arc::from("2023-01-15")),
            source_version_hint: None,
            provider_hints: vec![],
            descriptive_facts: None,
            episode_count: Some(1),
            image_ref: Some(MediaServerImageRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("season-2"),
                image_key: Arc::from("image-season-2"),
            }),
        },
    ]
}

fn expected_episodes(input_name: &Arc<str>, server_id: &Arc<str>) -> Vec<MediaServerEpisode> {
    vec![
        MediaServerEpisode {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(server_id),
            library_id: Arc::from("lib-tv"),
            item_id: Arc::from("ep-1"),
            season_id: Some(Arc::from("season-1")),
            series_id: Some(Arc::from("series-1")),
            series_title: Some(Arc::from("Epic Series")),
            title: Arc::from("Pilot"),
            season: Some(1),
            episode: Some(1),
            release_date: Some(Arc::from("2022-01-10")),
            source_version_hint: None,
            provider_hints: vec![],
            descriptive_facts: None,
            technical_facts: None,
            stream_ref: Some(MediaServerStreamRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("ep-1"),
                stream_key: Some(Arc::from("stream-ep-1")),
            }),
            image_ref: Some(MediaServerImageRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("ep-1"),
                image_key: Arc::from("image-ep-1"),
            }),
        },
        MediaServerEpisode {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(server_id),
            library_id: Arc::from("lib-tv"),
            item_id: Arc::from("ep-2"),
            season_id: Some(Arc::from("season-1")),
            series_id: Some(Arc::from("series-1")),
            series_title: Some(Arc::from("Epic Series")),
            title: Arc::from("Second Chapter"),
            season: Some(1),
            episode: Some(2),
            release_date: Some(Arc::from("2022-01-17")),
            source_version_hint: None,
            provider_hints: vec![],
            descriptive_facts: None,
            technical_facts: None,
            stream_ref: Some(MediaServerStreamRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("ep-2"),
                stream_key: Some(Arc::from("stream-ep-2")),
            }),
            image_ref: Some(MediaServerImageRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("ep-2"),
                image_key: Arc::from("image-ep-2"),
            }),
        },
        MediaServerEpisode {
            input_name: Arc::clone(input_name),
            server_id: Arc::clone(server_id),
            library_id: Arc::from("lib-tv"),
            item_id: Arc::from("ep-3"),
            season_id: Some(Arc::from("season-2")),
            series_id: Some(Arc::from("series-1")),
            series_title: Some(Arc::from("Epic Series")),
            title: Arc::from("New Beginning"),
            season: Some(2),
            episode: Some(1),
            release_date: Some(Arc::from("2023-01-15")),
            source_version_hint: None,
            provider_hints: vec![],
            descriptive_facts: None,
            technical_facts: None,
            stream_ref: Some(MediaServerStreamRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("ep-3"),
                stream_key: Some(Arc::from("stream-ep-3")),
            }),
            image_ref: Some(MediaServerImageRef::Plugin {
                input_name: Arc::clone(input_name),
                plugin_id: Arc::clone(server_id),
                item_id: Arc::from("ep-3"),
                image_key: Arc::from("image-ep-3"),
            }),
        },
    ]
}

fn build_expected_snapshot() -> MediaServerCatalogSnapshot {
    let input_name: Arc<str> = Arc::from("feed");
    let server_id: Arc<str> = Arc::from("media-probe");

    let libraries = expected_libraries(&input_name, &server_id);
    let movies = expected_movies(&input_name, &server_id);
    let series = expected_series(&input_name, &server_id);
    let seasons = expected_seasons(&input_name, &server_id);
    let episodes = expected_episodes(&input_name, &server_id);

    MediaServerCatalogSnapshot { libraries, movies, series, seasons, episodes, unsupported_libraries: vec![] }
}

#[tokio::test]
async fn golden_acceptance_a3_mapped_playlist_matches_core_mapper() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    // 1. Fetch snapshot from plugin
    let snapshot_from_plugin = probe
        .system
        .fetch_media_catalog_snapshot("media-probe", &ctx, &request(&[]))
        .await
        .expect("fetch_media_catalog_snapshot");

    let expected_snapshot = build_expected_snapshot();
    assert_eq!(snapshot_from_plugin, expected_snapshot, "plugin snapshot equals expected snapshot");

    // 2. Fetch mapped playlist via plugin host
    let mapped_via_plugin =
        probe.system.fetch_media_catalog("media-probe", &ctx, &request(&[])).await.expect("fetch_media_catalog");

    // 3. Map snapshot directly using core mapper
    let direct_mapped = media_server_catalog_snapshot_to_playlist(&expected_snapshot);

    // 4. Acceptance A3: mapped via plugin is field-equal to core mapper applied directly
    assert_groups_equal(&mapped_via_plugin, &direct_mapped);

    // Verify key fields in mapped output
    assert_eq!(mapped_via_plugin.len(), 2);
    let movie_group =
        mapped_via_plugin.iter().find(|g| g.xtream_cluster == XtreamCluster::Video).expect("movies group");
    assert_eq!(movie_group.channels.len(), 1);
    let movie_item = &movie_group.channels[0];
    assert_eq!(&*movie_item.header.name, "The Big Movie");
    assert_eq!(movie_item.header.item_type, PlaylistItemType::Video);
    assert_eq!(&*movie_item.header.url, "media-server://plugin/media-probe/movie-1?stream_key=stream-movie-1");

    let tv_group = mapped_via_plugin.iter().find(|g| g.xtream_cluster == XtreamCluster::Series).expect("tv group");
    // SeriesInfo + 3 Episodes = 4 channels
    assert_eq!(tv_group.channels.len(), 4);
    let series_info =
        tv_group.channels.iter().find(|c| c.header.item_type == PlaylistItemType::SeriesInfo).expect("series info");
    assert_eq!(&*series_info.header.name, "Epic Series");
}

#[tokio::test]
async fn paged_catalog_fetches_and_matches_normal() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let normal_playlist =
        probe.system.fetch_media_catalog("media-probe", &ctx, &request(&[])).await.expect("normal fetch");

    let paged_playlist = probe
        .system
        .fetch_media_catalog("media-probe", &ctx, &request(&[("mode".into(), "paged".into())]))
        .await
        .expect("paged fetch");

    assert_groups_equal(&paged_playlist, &normal_playlist);
}

#[tokio::test]
async fn evil_trap_fails_cleanly() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let result =
        probe.system.fetch_media_catalog("media-probe", &ctx, &request(&[("mode".into(), "trap".into())])).await;

    assert!(result.is_err(), "evil trap mode must return Err");
    let err = result.unwrap_err();
    assert!(err.contains("trap") || err.contains("unreachable"), "{err}");
}

#[tokio::test]
async fn evil_hang_fails_at_provider_deadline() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let started = std::time::Instant::now();
    let result =
        probe.system.fetch_media_catalog("media-probe", &ctx, &request(&[("mode".into(), "hang".into())])).await;

    assert!(result.is_err(), "evil hang mode must return Err");
    let elapsed = started.elapsed();
    // Verify it terminated without hanging indefinitely
    assert!(elapsed < std::time::Duration::from_secs(65), "hang was cut off by deadline");
}

#[tokio::test]
async fn evil_loop_cursor_detected() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let result =
        probe.system.fetch_media_catalog("media-probe", &ctx, &request(&[("mode".into(), "loop_cursor".into())])).await;

    assert!(result.is_err(), "evil loop cursor must return Err");
    let err = result.unwrap_err();
    assert!(err.contains("repeating cursor loop"), "{err}");
}

#[tokio::test]
async fn evil_oversized_clamped() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let snapshot = probe
        .system
        .fetch_media_catalog_snapshot("media-probe", &ctx, &request(&[("mode".into(), "oversized".into())]))
        .await
        .expect("oversized snapshot should be clamped cleanly without panicking");

    assert_eq!(snapshot.movies.len(), 1);
    let movie = &snapshot.movies[0];
    assert!(movie.title.len() <= 512, "title must be clamped to <= 512");
    assert!(movie.provider_hints.len() <= 50, "provider hints clamped to <= 50");
    if let Some(desc) = &movie.descriptive_facts {
        if let Some(summary) = &desc.summary {
            assert!(summary.len() <= 16384, "summary clamped to <= 16384");
        }
        assert!(desc.genres.len() <= 50, "genres clamped to <= 50");
        assert!(desc.countries.len() <= 50, "countries clamped to <= 50");
        assert!(desc.directors.len() <= 50, "directors clamped to <= 50");
        assert!(desc.writers.len() <= 50, "writers clamped to <= 50");
        assert!(desc.cast.len() <= 100, "cast clamped to <= 100");
        assert!(desc.crew.len() <= 100, "crew clamped to <= 100");
    }
}

#[tokio::test]
async fn evil_duplicate_ids_dropped() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let snapshot = probe
        .system
        .fetch_media_catalog_snapshot("media-probe", &ctx, &request(&[("mode".into(), "duplicate_ids".into())]))
        .await
        .expect("duplicate ids snapshot");

    assert_eq!(snapshot.movies.len(), 1, "duplicate movie id was dropped");
    assert_eq!(snapshot.series.len(), 1, "duplicate series id was dropped");
    assert_eq!(&*snapshot.movies[0].title, "The Big Movie");
    assert_eq!(&*snapshot.series[0].title, "Epic Series");
}

#[tokio::test]
async fn evil_dangling_ref_dropped() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    let snapshot = probe
        .system
        .fetch_media_catalog_snapshot("media-probe", &ctx, &request(&[("mode".into(), "dangling_ref".into())]))
        .await
        .expect("dangling ref snapshot");

    assert_eq!(snapshot.movies.len(), 0, "movie referencing unknown library was dropped");
    assert_eq!(snapshot.series.len(), 0, "series referencing unknown library was dropped");
    assert_eq!(snapshot.seasons.len(), 0, "season referencing unknown series was dropped");
    assert_eq!(snapshot.episodes.len(), 0, "episode referencing unknown series was dropped");
}

#[test]
fn internal_url_round_trips() {
    let input_name: Arc<str> = Arc::from("my-input");
    // 1. Stream ref round-trip
    let stream_ref = MediaServerStreamRef::Plugin {
        input_name: Arc::clone(&input_name),
        plugin_id: Arc::from("my-plugin"),
        item_id: Arc::from("movie-42"),
        stream_key: Some(Arc::from("key-abc")),
    };
    let url = media_server_stream_ref_to_internal_url(&stream_ref);
    assert_eq!(url, "media-server://plugin/my-plugin/movie-42?stream_key=key-abc");
    let parsed = parse_media_server_stream_ref(&input_name, &url).expect("parse stream ref");
    assert_eq!(parsed, stream_ref);

    // Stream ref with no stream_key
    let stream_ref_no_key = MediaServerStreamRef::Plugin {
        input_name: Arc::clone(&input_name),
        plugin_id: Arc::from("my-plugin"),
        item_id: Arc::from("movie-42"),
        stream_key: None,
    };
    let url_no_key = media_server_stream_ref_to_internal_url(&stream_ref_no_key);
    assert_eq!(url_no_key, "media-server://plugin/my-plugin/movie-42");
    let parsed_no_key = parse_media_server_stream_ref(&input_name, &url_no_key).expect("parse stream ref no key");
    assert_eq!(parsed_no_key, stream_ref_no_key);

    // 2. Image ref round-trip
    let image_ref = MediaServerImageRef::Plugin {
        input_name: Arc::clone(&input_name),
        plugin_id: Arc::from("my-plugin"),
        item_id: Arc::from("movie-42"),
        image_key: Arc::from("poster-xyz"),
    };
    let image_url = media_server_image_ref_to_internal_url(&image_ref);
    assert_eq!(image_url, "media-server://image/plugin/my-input/my-plugin/movie-42?image_key=poster-xyz");
    let parsed_image = parse_media_server_image_ref(&image_url).expect("parse image ref");
    assert_eq!(parsed_image, image_ref);
}

#[test]
fn both_exports_rejected_at_load() {
    let input_provider = "tuliprox:plugin/input-provider@1.0.0".to_string();
    let media_provider = "tuliprox:plugin/media-provider-v1-1@1.0.0".to_string();

    // Singly allowed
    assert!(check_conflicting_exports(std::slice::from_ref(&input_provider)).is_ok());
    assert!(check_conflicting_exports(std::slice::from_ref(&media_provider)).is_ok());

    // Both simultaneously rejected with clear error message
    let err = check_conflicting_exports(&[input_provider, media_provider]).unwrap_err();
    assert_eq!(
        err,
        "component exports both `input-provider` and `media-provider-v1-1`; at most one input provider interface is allowed"
    );
}

#[tokio::test]
async fn v1_input_provider_unchanged() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let items = probe
        .system
        .fetch_playlist(
            "provider-probe",
            &ctx,
            &PluginInputRequest {
                input_name: "feed",
                url: "http://host/feed",
                username: None,
                password: None,
                options: &[],
                refresh: false,
                last_success_at: None,
                event_sink: None,
                run_id: None,
                execution_order: None,
                input_id: None,
            },
        )
        .await
        .expect("v1 input provider fetch_playlist must succeed");

    assert_eq!(items.len(), 5, "default 5 deterministic channels returned");
    assert_eq!(&*items[0].header.name, "ch-000");
}

#[tokio::test]
async fn resolve_stream_normal() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let req = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "normal".into())],
        item_id: "movie-1",
        stream_key: Some("stream-movie-1"),
        range: Some("bytes=0-100"),
    };
    let target = probe.system.resolve_stream("media-probe", &ctx, &req).await.expect("resolve stream normal");
    assert_eq!(target.url, "http://example.com/media.mp4");
    assert_eq!(target.headers, vec![("X-Media-Probe-Auth".into(), "probe-secret-token".into())]);
    assert_eq!(target.ttl_secs, None);
}

#[tokio::test]
async fn resolve_stream_cached_with_ttl() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let req = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "ttl".into())],
        item_id: "movie-ttl",
        stream_key: Some("key-1"),
        range: None,
    };
    let target1 = probe.system.resolve_stream("media-probe", &ctx, &req).await.expect("resolve stream ttl");
    assert_eq!(target1.ttl_secs, Some(30));

    // Second call for the same key is served from cache
    let target2 = probe.system.resolve_stream("media-probe", &ctx, &req).await.expect("resolve stream cached");
    assert_eq!(target2.url, target1.url);
    assert_eq!(target2.headers, target1.headers);
}

#[tokio::test]
async fn resolve_stream_hang_deadline_stops_cleanly_and_host_survives() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let req = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "hang".into())],
        item_id: "movie-hang",
        stream_key: None,
        range: None,
    };
    let started = std::time::Instant::now();
    let result = probe.system.resolve_stream("media-probe", &ctx, &req).await;
    assert!(result.is_err(), "hang must fail with timeout error");
    let elapsed = started.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(10), "must time out within configured resolve deadline");

    // Subsequent normal resolve succeeds - host stays up and healthy!
    let normal_req = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "normal".into())],
        item_id: "movie-after-hang",
        stream_key: None,
        range: None,
    };
    let normal_target =
        probe.system.resolve_stream("media-probe", &ctx, &normal_req).await.expect("host survives hang");
    assert_eq!(normal_target.url, "http://example.com/media.mp4");
}

#[tokio::test]
async fn resolve_stream_trap_fails_cleanly_and_host_survives() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let req = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "trap".into())],
        item_id: "movie-trap",
        stream_key: None,
        range: None,
    };
    let result = probe.system.resolve_stream("media-probe", &ctx, &req).await;
    assert!(result.is_err(), "trap mode must fail");
    let err = result.unwrap_err();
    assert!(err.contains("trap") || err.contains("unreachable") || err.contains("simulated"), "{err}");

    // Host remains healthy
    let normal_req = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "normal".into())],
        item_id: "movie-after-trap",
        stream_key: None,
        range: None,
    };
    assert!(probe.system.resolve_stream("media-probe", &ctx, &normal_req).await.is_ok());
}

#[tokio::test]
async fn resolve_stream_scheme_and_host_validation() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    // Non-http scheme (ftp://) is rejected
    let req_non_http = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "non-http scheme".into())],
        item_id: "movie-non-http",
        stream_key: None,
        range: None,
    };
    let err = probe.system.resolve_stream("media-probe", &ctx, &req_non_http).await.unwrap_err();
    assert!(err.contains("only http and https are allowed"), "{err}");

    // Missing host (http://) is rejected
    let req_missing_host = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "private/disallowed host".into())],
        item_id: "movie-missing-host",
        stream_key: None,
        range: None,
    };
    let err = probe.system.resolve_stream("media-probe", &ctx, &req_missing_host).await.unwrap_err();
    assert!(err.contains("empty host") || err.contains("missing a valid host"), "{err}");
}

#[tokio::test]
async fn resolve_stream_header_checks() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    // Denylisted headers (Host, Connection)
    let req_denied = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "oversized and denylisted headers".into())],
        item_id: "movie-denied-hdr",
        stream_key: None,
        range: None,
    };
    let err = probe.system.resolve_stream("media-probe", &ctx, &req_denied).await.unwrap_err();
    assert!(err.contains("denylist"), "{err}");

    // Oversized headers count (> 32)
    let req_oversized = PluginStreamRequest {
        input_name: "feed",
        url: "http://example.com/media.mp4",
        username: None,
        password: None,
        options: &[("mode".into(), "oversized".into())],
        item_id: "movie-oversized-hdr",
        stream_key: None,
        range: None,
    };
    let err = probe.system.resolve_stream("media-probe", &ctx, &req_oversized).await.unwrap_err();
    assert!(err.contains("exceeds cap"), "{err}");
}

#[tokio::test]
async fn resolve_image_normal_and_error() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    // Normal image resolve
    let req = PluginImageRequest {
        input_name: "feed",
        url: "http://example.com/poster.jpg",
        username: None,
        password: None,
        options: &[("image_mode".into(), "normal".into())],
        item_id: "movie-1",
        image_key: "image-movie-1",
    };
    let target = probe.system.resolve_image("media-probe", &ctx, &req).await.expect("resolve image normal");
    assert_eq!(target.url, "http://example.com/poster.jpg");
    assert_eq!(target.headers, vec![("X-Media-Probe-Auth".into(), "probe-secret-token".into())]);

    // Error image resolve (trap)
    let req_trap = PluginImageRequest {
        input_name: "feed",
        url: "http://example.com/poster.jpg",
        username: None,
        password: None,
        options: &[("image_mode".into(), "trap".into())],
        item_id: "movie-trap",
        image_key: "image-trap",
    };
    let result = probe.system.resolve_image("media-probe", &ctx, &req_trap).await;
    assert!(result.is_err(), "image trap must fail");
}

#[tokio::test]
async fn normal_vs_refresh_reaches_the_guest() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    // NORMAL: refresh is false
    let opts_normal = [("mode".into(), "echo_request".into())];
    let mut req_normal = request(&opts_normal);
    req_normal.refresh = false;
    let snapshot =
        probe.system.fetch_media_catalog_snapshot("media-probe", &ctx, &req_normal).await.expect("normal fetch");
    assert!(
        snapshot.movies[0].title.starts_with("refresh:false"),
        "expected refresh:false, got: {}",
        snapshot.movies[0].title
    );

    // REFRESH: refresh is true
    let opts_refresh = [("mode".into(), "echo_request".into())];
    let mut req_refresh = request(&opts_refresh);
    req_refresh.refresh = true;
    let snapshot =
        probe.system.fetch_media_catalog_snapshot("media-probe", &ctx, &req_refresh).await.expect("refresh fetch");
    assert!(
        snapshot.movies[0].title.starts_with("refresh:true"),
        "expected refresh:true, got: {}",
        snapshot.movies[0].title
    );
}

#[tokio::test]
async fn last_success_at_is_none_on_first_run_and_previous_success_time_on_next_run() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");

    // First run: last_success_at is None
    let opts1 = [("mode".into(), "echo_request".into())];
    let mut req1 = request(&opts1);
    req1.last_success_at = None;
    let snapshot1 = probe.system.fetch_media_catalog_snapshot("media-probe", &ctx, &req1).await.expect("first fetch");
    assert!(
        snapshot1.movies[0].title.contains("last_success_at:none"),
        "expected last_success_at:none, got: {}",
        snapshot1.movies[0].title
    );

    // Next run: last_success_at is previous success timestamp
    let opts2 = [("mode".into(), "echo_request".into())];
    let mut req2 = request(&opts2);
    req2.last_success_at = Some(1_700_000_123);
    let snapshot2 = probe.system.fetch_media_catalog_snapshot("media-probe", &ctx, &req2).await.expect("second fetch");
    assert!(
        snapshot2.movies[0].title.contains("last_success_at:1700000123"),
        "expected last_success_at:1700000123, got: {}",
        snapshot2.movies[0].title
    );
}

struct TestProgressSink(Arc<std::sync::Mutex<Vec<shared::model::EventMessage>>>);

impl shared::model::EventSink for TestProgressSink {
    fn emit(&self, event: shared::model::EventMessage) { self.0.lock().unwrap().push(event); }
}

#[tokio::test]
async fn progress_events_emitted_for_3_page_catalog() {
    let Some(probe) = load_probe(vec![probe_entry("media-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let sink = TestProgressSink(Arc::new(std::sync::Mutex::new(Vec::new())));

    let opts = [("mode".into(), "paged".into())];
    let mut req = request(&opts);
    req.event_sink = Some(&sink);
    let run_id = shared::model::PlaylistUpdateRunId::from("catalog-update-run");
    req.run_id = Some(&run_id);
    req.execution_order = Some(1.into());
    req.input_id = Some(42);

    let groups = probe.system.fetch_media_catalog("media-probe", &ctx, &req).await.expect("paged fetch");
    assert!(!groups.is_empty(), "mapped groups must not be empty");

    let emitted = sink.0.lock().unwrap().clone();
    let progress_events: Vec<_> = emitted
        .into_iter()
        .filter_map(|e| match e {
            shared::model::EventMessage::PlaylistUpdateProgress(p) => Some(p),
            _ => None,
        })
        .collect();

    assert_eq!(progress_events.len(), 3, "expected exactly 3 progress events for 3 pages, got: {progress_events:?}");
    for (idx, event) in progress_events.iter().enumerate() {
        let expected_page = idx + 1;
        assert_eq!(event.input_id, Some(42));
        assert_eq!(event.target, "feed");
        assert!(
            event.message.contains(&format!("page {expected_page}")),
            "expected message to contain 'page {expected_page}', got: {}",
            event.message
        );
    }
}

#[tokio::test]
async fn v1_0_plugin_without_host_v1_1_import_still_loads() {
    let Some(probe) = load_probe(vec![probe_entry("provider-probe", serde_json::json!({}))], common::default_services)
    else {
        return;
    };
    let ctx = HookCtx::new("").with_input("feed");
    let req = request(&[]);
    let items = probe
        .system
        .fetch_playlist("provider-probe", &ctx, &req)
        .await
        .expect("v1.0 plugin must load and execute successfully without host-v1-1 import");
    assert_eq!(items.len(), 5);
}
