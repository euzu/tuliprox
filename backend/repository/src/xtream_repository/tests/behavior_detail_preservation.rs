use super::{
    behavior::read_live_props, fixed_refresh_paths, make_live_item, merge_preserved_stream_properties,
    needs_update_info_details, preserve_details_input_xtream_playlist_cluster_to_disk,
    preserve_details_with_injected_operation_failure, refresh_staging_path, write_detail_preservation_fixture,
    write_single_item, DetailPreservationOperation, PreserveDetailsOutcome,
};
use shared::{
    model::{CatchupProperties, LiveStreamProperties, SeriesStreamProperties, StreamProperties, VideoStreamProperties},
    utils::Internable,
};
use std::path::Path;
use tempfile::tempdir;
use uuid::Uuid;

#[test]
fn keeps_existing_details_when_new_timestamp_is_missing() {
    let new_props = StreamProperties::Video(Box::new(VideoStreamProperties {
        added: "".into(),
        ..VideoStreamProperties::default()
    }));
    let old_props = StreamProperties::Video(Box::new(VideoStreamProperties {
        added: "1700000000".into(),
        ..VideoStreamProperties::default()
    }));

    assert!(!needs_update_info_details(&new_props, &old_props));
}

#[test]
fn updates_details_when_new_timestamp_is_newer() {
    let new_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
        last_modified: Some("200".into()),
        ..SeriesStreamProperties::default()
    }));
    let old_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
        last_modified: Some("100".into()),
        ..SeriesStreamProperties::default()
    }));

    assert!(needs_update_info_details(&new_props, &old_props));
}

#[test]
fn does_not_update_details_when_new_timestamp_is_older() {
    let new_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
        last_modified: Some("100".into()),
        ..SeriesStreamProperties::default()
    }));
    let old_props = StreamProperties::Series(Box::new(SeriesStreamProperties {
        last_modified: Some("200".into()),
        ..SeriesStreamProperties::default()
    }));

    assert!(!needs_update_info_details(&new_props, &old_props));
}

#[test]
fn merge_preserves_missing_live_probe_timestamps() {
    let mut new_props =
        StreamProperties::Live(Box::new(LiveStreamProperties { stream_id: 1, ..LiveStreamProperties::default() }));
    let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
        stream_id: 1,
        last_probed_timestamp: Some(1_700_000_000),
        last_success_timestamp: Some(1_700_000_100),
        ..LiveStreamProperties::default()
    }));

    let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
    assert!(changed);

    match new_props {
        StreamProperties::Live(live) => {
            assert_eq!(live.last_probed_timestamp, Some(1_700_000_000));
            assert_eq!(live.last_success_timestamp, Some(1_700_000_100));
        }
        _ => panic!("expected live properties"),
    }
}

#[test]
fn merge_does_not_override_existing_live_probe_timestamps() {
    let mut new_props = StreamProperties::Live(Box::new(LiveStreamProperties {
        stream_id: 1,
        last_probed_timestamp: Some(1_800_000_000),
        last_success_timestamp: Some(1_800_000_100),
        ..LiveStreamProperties::default()
    }));
    let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
        stream_id: 1,
        last_probed_timestamp: Some(1_700_000_000),
        last_success_timestamp: Some(1_700_000_100),
        ..LiveStreamProperties::default()
    }));

    let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
    assert!(!changed);

    match new_props {
        StreamProperties::Live(live) => {
            assert_eq!(live.last_probed_timestamp, Some(1_800_000_000));
            assert_eq!(live.last_success_timestamp, Some(1_800_000_100));
        }
        _ => panic!("expected live properties"),
    }
}

#[test]
fn merge_preserves_higher_learned_live_bitrate() {
    let mut new_props = StreamProperties::Live(Box::new(LiveStreamProperties {
        stream_id: 1,
        bitrate: 1_500_000,
        ..LiveStreamProperties::default()
    }));
    let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
        stream_id: 1,
        bitrate: 2_500_000,
        ..LiveStreamProperties::default()
    }));

    assert!(merge_preserved_stream_properties(&mut new_props, &old_props));
    match new_props {
        StreamProperties::Live(live) => assert_eq!(live.bitrate, 2_500_000),
        _ => panic!("expected live properties"),
    }
}

#[test]
fn merge_preserves_missing_live_catchup_properties() {
    let mut new_props =
        StreamProperties::Live(Box::new(LiveStreamProperties { stream_id: 1, ..LiveStreamProperties::default() }));
    let old_props = StreamProperties::Live(Box::new(LiveStreamProperties {
        stream_id: 1,
        catchup: Some(CatchupProperties {
            mode: Some("append".into()),
            source: Some("?offset=-${offset}".into()),
            ..CatchupProperties::default()
        }),
        ..LiveStreamProperties::default()
    }));

    let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
    assert!(changed);
    match new_props {
        StreamProperties::Live(live) => {
            let catchup = live.catchup.expect("catchup should be preserved");
            assert_eq!(catchup.mode.as_deref(), Some("append"));
            assert_eq!(catchup.source.as_deref(), Some("?offset=-${offset}"));
        }
        _ => panic!("expected live properties"),
    }
}

#[test]
fn merge_preserves_missing_video_tmdb() {
    let mut new_props =
        StreamProperties::Video(Box::new(VideoStreamProperties { tmdb: None, ..VideoStreamProperties::default() }));
    let old_props = StreamProperties::Video(Box::new(VideoStreamProperties {
        tmdb: Some(317_981),
        ..VideoStreamProperties::default()
    }));

    let changed = merge_preserved_stream_properties(&mut new_props, &old_props);
    assert!(changed);
    match new_props {
        StreamProperties::Video(video) => assert_eq!(video.tmdb, Some(317_981)),
        _ => panic!("expected video properties"),
    }
}

#[test]
fn preserve_details_for_disk_cluster_copies_missing_live_probe_fields() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 3);
    let provider_id = 100_u32;

    write_single_item(
        &paths.published_database,
        &make_live_item(
            provider_id,
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some(1_700_000_000),
            Some(1_700_000_100),
            2_500_000,
        ),
    );
    write_single_item(&paths.staging_database, &make_live_item(provider_id, None, None, None, None, 0));

    let outcome =
        preserve_details_input_xtream_playlist_cluster_to_disk(&paths.published_database, &paths.staging_database)
            .expect("merge should succeed");
    assert_eq!(outcome, PreserveDetailsOutcome::Merged { scanned: 1, updated: 1 });

    let merged = read_live_props(&paths.staging_database, provider_id);
    assert_eq!(merged.video, Some("{\"codec_name\":\"h264\"}".intern()));
    assert_eq!(merged.audio, Some("{\"codec_name\":\"aac\"}".intern()));
    assert_eq!(merged.last_probed_timestamp, Some(1_700_000_000));
    assert_eq!(merged.last_success_timestamp, Some(1_700_000_100));
    assert_eq!(merged.bitrate, 2_500_000);
}

#[test]
fn preserve_details_reports_missing_published_database_explicitly() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 4);
    write_single_item(&paths.staging_database, &make_live_item(101, None, None, None, None, 0));

    let outcome =
        preserve_details_input_xtream_playlist_cluster_to_disk(&paths.published_database, &paths.staging_database)
            .expect("a missing published database should not fail the refresh");

    assert_eq!(outcome, PreserveDetailsOutcome::SourceMissing);
}

#[test]
fn preserve_details_propagates_missing_staging_database() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 14);
    write_single_item(&paths.published_database, &make_live_item(105, None, None, None, None, 0));

    let error =
        preserve_details_input_xtream_playlist_cluster_to_disk(&paths.published_database, &paths.staging_database)
            .expect_err("a missing staging database must fail");
    let message = error.to_string();

    assert!(message.contains("Failed to open staging Xtream tree"));
    assert!(message.contains(&paths.staging_database.display().to_string()));
}

#[test]
fn preserve_details_propagates_staging_batch_write_failure() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 16);
    write_detail_preservation_fixture(&paths, 107);

    let error = preserve_details_with_injected_operation_failure(
        &paths.published_database,
        &paths.staging_database,
        DetailPreservationOperation::BatchWrite,
    )
    .expect_err("a staging batch write failure must fail the merge");
    let message = error.to_string();

    assert!(message.contains("Failed to update staging Xtream tree"));
    assert!(message.contains(&paths.staging_database.display().to_string()));
    assert!(message.contains("injected BatchWrite failure"));
}

#[test]
fn preserve_details_empty_merge_reports_zero_updates() {
    let dir = tempdir().expect("temp dir should be created");
    let paths = fixed_refresh_paths(dir.path(), 7);
    let item = make_live_item(104, None, None, None, None, 0);
    write_single_item(&paths.published_database, &item);
    write_single_item(&paths.staging_database, &item);

    let outcome =
        preserve_details_input_xtream_playlist_cluster_to_disk(&paths.published_database, &paths.staging_database)
            .expect("empty merge should succeed");

    assert_eq!(outcome, PreserveDetailsOutcome::Merged { scanned: 1, updated: 0 });
}

#[cfg(unix)]
#[test]
fn refresh_staging_path_preserves_non_utf8_stem() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut input_name = std::ffi::OsString::from_vec(vec![b'l', b'i', b'v', b'e', 0xff]);
    input_name.push(".db");
    let published = Path::new("/tmp").join(input_name);
    let generation = Uuid::from_u128(12);

    let staging = refresh_staging_path(&published, generation).expect("non-UTF-8 path should be supported");
    let bytes = staging.file_name().expect("staging file name").as_bytes();
    assert!(bytes.starts_with(&[b'l', b'i', b'v', b'e', 0xff]));
    assert!(bytes.ends_with(b".db"));
    assert!(bytes.windows(b".refresh-".len()).any(|window| window == b".refresh-"));
}

#[test]
fn preserve_details_for_disk_cluster_does_not_override_existing_live_probe_fields() {
    let dir = tempdir().expect("temp dir should be created");
    let old_path = dir.path().join("old_live_existing.db");
    let tmp_path = dir.path().join("tmp_live_existing.db");
    let provider_id = 200_u32;

    write_single_item(
        &old_path,
        &make_live_item(
            provider_id,
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some(1_700_000_000),
            Some(1_700_000_100),
            2_500_000,
        ),
    );
    write_single_item(
        &tmp_path,
        &make_live_item(
            provider_id,
            Some("{\"codec_name\":\"hevc\"}"),
            Some("{\"codec_name\":\"ac3\"}"),
            Some(1_800_000_000),
            Some(1_800_000_100),
            3_500_000,
        ),
    );

    preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &tmp_path).expect("merge should succeed");

    let merged = read_live_props(&tmp_path, provider_id);
    assert_eq!(merged.video, Some("{\"codec_name\":\"hevc\"}".intern()));
    assert_eq!(merged.audio, Some("{\"codec_name\":\"ac3\"}".intern()));
    assert_eq!(merged.last_probed_timestamp, Some(1_800_000_000));
    assert_eq!(merged.last_success_timestamp, Some(1_800_000_100));
    assert_eq!(merged.bitrate, 3_500_000);
}

#[test]
fn preserve_details_for_disk_cluster_fills_only_missing_live_probe_fields() {
    let dir = tempdir().expect("temp dir should be created");
    let old_path = dir.path().join("old_live_partial.db");
    let tmp_path = dir.path().join("tmp_live_partial.db");
    let provider_id = 300_u32;

    write_single_item(
        &old_path,
        &make_live_item(
            provider_id,
            Some("{\"codec_name\":\"h264\"}"),
            Some("{\"codec_name\":\"aac\"}"),
            Some(1_700_000_000),
            Some(1_700_000_100),
            2_500_000,
        ),
    );
    write_single_item(
        &tmp_path,
        &make_live_item(provider_id, Some("{\"codec_name\":\"hevc\"}"), None, Some(1_800_000_000), None, 3_500_000),
    );

    preserve_details_input_xtream_playlist_cluster_to_disk(&old_path, &tmp_path).expect("merge should succeed");

    let merged = read_live_props(&tmp_path, provider_id);
    assert_eq!(merged.video, Some("{\"codec_name\":\"hevc\"}".intern()));
    assert_eq!(merged.audio, Some("{\"codec_name\":\"aac\"}".intern()));
    assert_eq!(merged.last_probed_timestamp, Some(1_800_000_000));
    assert_eq!(merged.last_success_timestamp, Some(1_700_000_100));
}
