use super::*;
use crate::model::{PlaylistItemType, VirtualId, XtreamCluster, XtreamPlaylistItem, XtreamSeriesInfo, XtreamVideoInfo};
use serde_json::json;

#[test]
fn genre_reads_from_wherever_the_variant_keeps_it() {
    let mut video = StreamProperties::Video(Box::default());
    assert_eq!(video.genre(), None, "video with no details has no genre");
    assert!(video.set_genre("Action"), "video accepts a genre");
    assert_eq!(video.genre().map(Arc::as_ref), Some("Action"), "video keeps it under details");
    // Setting again goes through the existing details rather than replacing them.
    assert!(video.set_genre("Drama"));
    assert_eq!(video.genre().map(Arc::as_ref), Some("Drama"));

    let mut series = StreamProperties::Series(Box::default());
    assert_eq!(series.genre(), None);
    assert!(series.set_genre("Comedy"), "series accepts a genre");
    assert_eq!(series.genre().map(Arc::as_ref), Some("Comedy"), "series keeps it inline");
}

#[test]
fn live_and_episode_have_no_genre_and_refuse_to_take_one() {
    // EpisodeStreamProperties has no Default impl, but every field is
    // #[serde(default)], so an empty object builds one.
    let episode: EpisodeStreamProperties =
        serde_json::from_value(json!({})).expect("episode properties default from an empty object");
    for mut props in [StreamProperties::Live(Box::default()), StreamProperties::Episode(Box::new(episode))] {
        assert_eq!(props.genre(), None);
        assert!(!props.set_genre("Action"), "this variant has no genre to set");
        assert_eq!(props.genre(), None);
    }
}

fn sample_video_info(tmdb_id: &str) -> XtreamVideoInfo {
    serde_json::from_value(json!({
        "info": {
            "name": "Fetched Movie",
            "tmdb_id": tmdb_id,
            "movie_image": "https://img.example/movie.jpg",
            "video": { "codec": "h264" },
            "audio": { "codec": "aac" }
        },
        "movie_data": {
            "name": "Fetched Movie",
            "category_id": "99",
            "stream_id": "4242",
            "direct_source": "https://cdn.example/stream.mkv",
            "added": "1700000000",
            "container_extension": "mkv"
        }
    }))
    .expect("sample XtreamVideoInfo should deserialize")
}

fn existing_video_item() -> XtreamPlaylistItem {
    XtreamPlaylistItem {
        virtual_id: VirtualId::new(1),
        provider_id: 1001,
        name: "Existing".into(),
        logo: "".into(),
        logo_small: "".into(),
        group: "".into(),
        title: "Existing title".into(),
        parent_code: "".into(),
        rec: "".into(),
        url: "https://provider.example/stream".into(),
        epg_channel_id: None,
        xtream_cluster: XtreamCluster::Video,
        additional_properties: Some(StreamProperties::Video(Box::new(VideoStreamProperties {
            rating: Some(7.5),
            rating_5based: Some(3.7),
            stream_type: Some("legacy-type".into()),
            trailer: Some("legacy-trailer".into()),
            tmdb: Some(7788),
            is_adult: 1,
            ..Default::default()
        }))),
        item_type: PlaylistItemType::Video,
        category_id: 99,
        input_name: "input".into(),
        channel_no: 0,
        source_ordinal: 0,
        input_stream_id: "1001".into(),
        upstream_user_agent: None,
    }
}

#[test]
fn from_info_without_existing_keeps_fetched_metadata() {
    let info = sample_video_info("12345");

    let props = VideoStreamProperties::from_info_without_existing(&info);
    let details = props.details.expect("details should be present");

    assert_eq!(props.name.as_ref(), "Fetched Movie");
    assert_eq!(props.stream_id, 4242);
    assert_eq!(props.tmdb, Some(12345));
    assert_eq!(props.stream_type.as_deref(), Some("movie"));
    assert!(details.video.is_some());
    assert!(details.audio.is_some());
}

#[test]
fn from_info_with_existing_keeps_existing_overrides() {
    let info = sample_video_info("");
    let existing = existing_video_item();

    let props = VideoStreamProperties::from_info(&info, &existing);

    assert_eq!(props.rating, Some(7.5));
    assert_eq!(props.rating_5based, Some(3.7));
    assert_eq!(props.stream_type.as_deref(), Some("legacy-type"));
    assert_eq!(props.trailer.as_deref(), Some("legacy-trailer"));
    assert_eq!(props.tmdb, Some(7788));
    assert_eq!(props.is_adult, 1);
}

#[test]
fn from_series_info_preserves_episode_plot() {
    let info: XtreamSeriesInfo = serde_json::from_value(json!({
        "info": {
            "name": "Example Show"
        },
        "episodes": {
            "1": [{
                "id": 101,
                "episode_num": 1,
                "season": 1,
                "title": "Pilot",
                "container_extension": "mkv",
                "info": {
                    "air_date": "2024-01-01",
                    "plot": "Episode plot",
                    "movie_image": "https://img.example/still.jpg"
                }
            }]
        }
    }))
    .expect("sample XtreamSeriesInfo should deserialize");

    let props = SeriesStreamProperties::from_info_without_existing(&info, 7);
    let episode = props
        .details
        .and_then(|details| details.episodes)
        .and_then(|episodes| episodes.first().cloned())
        .expect("episode should be mapped");

    assert_eq!(episode.plot.as_deref(), Some("Episode plot"));
}

#[test]
fn normalize_episode_title_injects_missing_episode_code() {
    let normalized = normalize_episode_title(&"Pilot".into(), &"Example Show".into(), 1, 2);
    assert_eq!(normalized.as_ref(), "S01E02 - Pilot");
}

#[test]
fn normalize_episode_title_replaces_series_name_only_with_episode_code() {
    let normalized = normalize_episode_title(&"Example Show".into(), &"Example Show".into(), 1, 2);
    assert_eq!(normalized.as_ref(), "S01E02");
}

#[test]
fn normalize_episode_title_keeps_existing_episode_code() {
    let normalized = normalize_episode_title(&"S01E02".into(), &"Example Show".into(), 1, 2);
    assert_eq!(normalized.as_ref(), "S01E02");
}

#[test]
fn catchup_messagepack_round_trip_preserves_archive_window() -> Result<(), Box<dyn std::error::Error>> {
    for window in [None, Some(14400)] {
        let catchup = CatchupProperties {
            mode: Some("fs".into()),
            days: Some("10".into()),
            source: Some("https://host/channel/archive-{utc}-{duration}.m3u8?token=a%2Fb".into()),
            time: None,
            correction: Some("0".into()),
            catchup_type: Some("flussonic".into()),
            extra_attributes: vec![super::CatchupAttribute { name: "catchup-extra".into(), value: "keep".into() }],
            flussonic_archive_max_duration_secs: window,
        };
        let encoded = rmp_serde::to_vec(&catchup)?;
        let restored: CatchupProperties = rmp_serde::from_slice(&encoded)?;
        assert_eq!(restored, catchup);

        let properties = StreamProperties::Live(Box::new(LiveStreamProperties {
            catchup: Some(catchup),
            ..LiveStreamProperties::default()
        }));
        let encoded = rmp_serde::to_vec(&properties)?;
        let restored: StreamProperties = rmp_serde::from_slice(&encoded)?;
        assert_eq!(restored, properties);
    }
    Ok(())
}

#[test]
fn catchup_messagepack_reads_legacy_records_without_changing_encoding() -> Result<(), Box<dyn std::error::Error>> {
    // The seven-field positional layout is independent of the current struct definition.
    let legacy_fields = (
        Some("fs"),
        Some("10"),
        Some("https://host/channel/archive-{utc}-{duration}.m3u8?token=a%2Fb"),
        None::<&str>,
        Some("0"),
        Some("flussonic"),
        vec![("catchup-extra", "keep")],
    );
    let encoded = rmp_serde::to_vec(&legacy_fields)?;
    let restored: CatchupProperties = rmp_serde::from_slice(&encoded)?;
    assert_eq!(restored.flussonic_archive_max_duration_secs, None);
    assert_eq!(restored.mode.as_deref(), Some("fs"));
    assert_eq!(restored.days.as_deref(), Some("10"));
    assert_eq!(restored.source.as_deref(), legacy_fields.2);
    assert_eq!(restored.time, None);
    assert_eq!(restored.correction.as_deref(), Some("0"));
    assert_eq!(restored.catchup_type.as_deref(), Some("flussonic"));
    assert_eq!(
        restored.extra_attributes,
        vec![super::CatchupAttribute { name: "catchup-extra".into(), value: "keep".into() }]
    );
    assert_eq!(rmp_serde::to_vec(&restored)?, encoded);
    Ok(())
}

#[test]
fn live_catchup_only_counts_as_details() {
    let props = StreamProperties::Live(Box::new(LiveStreamProperties {
        catchup: Some(CatchupProperties { mode: Some("append".into()), ..CatchupProperties::default() }),
        ..LiveStreamProperties::default()
    }));

    assert!(props.has_details());
}

#[test]
fn native_flussonic_mode_is_canonical_and_respects_mode_precedence() {
    for (value, expected) in [
        ("fs", Some("flussonic")),
        ("FLUSSONIC", Some("flussonic")),
        ("flussonic-hls", Some("flussonic")),
        ("Flussonic-TS", Some("flussonic-ts")),
        ("append", None),
    ] {
        let catchup = CatchupProperties { mode: Some(value.into()), ..CatchupProperties::default() };
        assert_eq!(catchup.native_flussonic_player_mode(), expected);
    }

    // BitTV often ships catchup="append"/"shift" with catchup-type="flussonic".
    // catchup-type must win so path-rewrite archive stays Flussonic (v3.3.81).
    let conflicting = CatchupProperties {
        mode: Some("append".into()),
        catchup_type: Some("flussonic".into()),
        ..CatchupProperties::default()
    };
    assert_eq!(conflicting.effective_mode(), Some("flussonic"));
    assert!(conflicting.is_flussonic());
    assert_eq!(conflicting.native_flussonic_player_mode(), Some("flussonic"));

    let shift_type_wins = CatchupProperties {
        mode: Some("flussonic".into()),
        catchup_type: Some("shift".into()),
        ..CatchupProperties::default()
    };
    assert_eq!(shift_type_wins.effective_mode(), Some("shift"));
    assert!(!shift_type_wins.is_flussonic());
    assert_eq!(shift_type_wins.native_flussonic_player_mode(), None);

    let whitespace_type = CatchupProperties {
        mode: Some("append".into()),
        catchup_type: Some("  ".into()),
        ..CatchupProperties::default()
    };
    assert_eq!(whitespace_type.effective_mode(), Some("append"));
    assert_eq!(whitespace_type.append_player_type(), Some("append"));
}

#[test]
fn positive_live_bitrate_counts_as_details() {
    let props = StreamProperties::Live(Box::new(LiveStreamProperties {
        bitrate: 2_500_000,
        ..LiveStreamProperties::default()
    }));

    assert!(props.has_details());
}

#[test]
fn live_bitrate_deserializes_from_string_and_defaults_when_missing() {
    let measured: LiveStreamProperties =
        serde_json::from_value(json!({ "bitrate": "2500000" })).expect("string bitrate should deserialize");
    let unknown: LiveStreamProperties = serde_json::from_value(json!({})).expect("missing bitrate should deserialize");

    assert_eq!(measured.bitrate, 2_500_000);
    assert_eq!(unknown.bitrate, 0);
}

#[test]
fn merge_learned_live_metadata_preserves_existing_values_and_higher_bitrate() {
    let mut current = LiveStreamProperties {
        video: Some("current-video".into()),
        bitrate: 1_500_000,
        last_probed_timestamp: Some(200),
        ..LiveStreamProperties::default()
    };
    let previous = LiveStreamProperties {
        video: Some("previous-video".into()),
        audio: Some("previous-audio".into()),
        bitrate: 2_500_000,
        last_probed_timestamp: Some(100),
        last_success_timestamp: Some(150),
        ..LiveStreamProperties::default()
    };

    assert!(current.merge_learned_metadata_from(&previous));
    assert_eq!(current.video.as_deref(), Some("current-video"));
    assert_eq!(current.audio.as_deref(), Some("previous-audio"));
    assert_eq!(current.bitrate, 2_500_000);
    assert_eq!(current.last_probed_timestamp, Some(200));
    assert_eq!(current.last_success_timestamp, Some(150));
}

#[test]
fn resource_field_names_exclude_stream_urls() {
    for resource in [
        "logo",
        "logo_small",
        "cover",
        "movie_image",
        "nfo_cover_big",
        "backdrop_path",
        "backdrop_path1",
        "nfo_backdrop_path",
        "nfo_s_2_cover",
        "nfo_ep_2_5_movie_image",
    ] {
        assert!(is_resource_field_name(resource), "{resource}");
    }
    for not_a_resource in ["url", "name", "chno", "caption", "epg_channel_id", "stream_url", "nfo_s_2_title"] {
        assert!(!is_resource_field_name(not_a_resource), "{not_a_resource}");
    }
}
