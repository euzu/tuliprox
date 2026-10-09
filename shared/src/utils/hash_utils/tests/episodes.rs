use super::*;

#[test]
fn generate_local_playlist_uuid_keeps_local_series_info_and_episode_distinct() {
    let url = "file:///library/show/Season%201/Episode%2001.mkv";
    let series_info = generate_local_playlist_uuid("library", PlaylistItemType::LocalSeriesInfo, url);
    let episode = generate_local_playlist_uuid("library", PlaylistItemType::LocalSeries, url);

    assert_ne!(series_info, episode);
}

#[test]
fn stable_episode_storage_id_keys_separate_contexts() {
    // Same (series, season, episode_num, episode_id) tuple must hash
    // identically regardless of how the caller order is.
    let a = stable_episode_storage_id(1, 1, "10", 1);
    let b = stable_episode_storage_id(1, 1, "10", 1);
    assert_eq!(a, b);
    // Different episode_num must not collide.
    assert_ne!(a, stable_episode_storage_id(1, 1, "10", 2));
    // Different series must not collide.
    assert_ne!(a, stable_episode_storage_id(2, 1, "10", 1));
}

#[test]
fn parse_season_episode_extracts_with_default_pattern() {
    let pattern = Regex::new(EPISODE_PATTERN).unwrap();
    assert_eq!(parse_season_episode("Show S02E05 [1080p]", &pattern), Some((2, 5)));
    assert_eq!(parse_season_episode("Show s8e2", &pattern), Some((8, 2)));
    assert_eq!(parse_season_episode("Show without episode", &pattern), None);
    // Pattern only matches the digits grouped after S and E.
    assert_eq!(parse_season_episode("Pilot", &pattern), None);
}

#[test]
fn parse_season_episode_legacy_user_pattern_with_wrapped_episode_capture() {
    // A user-configured pattern that still wraps the whole
    // `SxxEyy` token inside a single named capture (the
    // pre-refactor format) must keep working — the function
    // falls back to splitting the captured string.
    let legacy = Regex::new(r".*(?P<episode>[Ss]\d{1,2}(.*?)[Ee]\d{1,2}).*").unwrap();
    assert_eq!(parse_season_episode("Show S02E05 [1080p]", &legacy), Some((2, 5)));
    assert_eq!(parse_season_episode("Show s8e2", &legacy), Some((8, 2)));
}

#[test]
fn parse_season_episode_handles_verbose_and_fallback_shapes() {
    // The CONSTANTS regex covers the four historical shapes.
    let pattern = Regex::new(CONSTANTS.re_episode_code.as_str()).unwrap();
    // Verbose `Season X Episode Y`
    assert_eq!(parse_season_episode("Show - Season 2 Episode 5", &pattern), Some((2, 5)),);
    assert_eq!(parse_season_episode("Show Season 02 Episode 05", &pattern), Some((2, 5)),);
    // `NxNN`
    assert_eq!(parse_season_episode("Show 1x05", &pattern), Some((1, 5)));
    // Bare `Episode Y` falls back to season 1.
    assert_eq!(parse_season_episode("Show Episode 7", &pattern), Some((1, 7)));
    // No match → None.
    assert_eq!(parse_season_episode("Pilot", &pattern), None);
}
