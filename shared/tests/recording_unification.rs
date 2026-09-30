use shared::model::{
    ConfigDto, RecordingConfigDto, RecordingDiskConfigDto, RecordingNotificationConfigDto, RecordingQuotaConfigDto,
    RecordingRetentionConfigDto, VideoConfigDto,
};

fn parse(yaml: &str) -> ConfigDto {
    serde_saphyr::from_str(yaml).unwrap_or_else(|error| panic!("config must parse: {error}"))
}

#[test]
fn video_round_trips_only_extensions_web_search_and_recording() {
    let yaml = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
video:
  extensions: [".ts", ".mp4"]
  web_search: https://example.test/search
  recording:
    headers: {Accept: "video/*"}
    directory: /var/lib/tuliprox/recordings
    organize_into_directories: true
    episode_pattern: ".*"
    priority: 7
"#;

    let mut config = parse(yaml);
    config.prepare(false).unwrap_or_else(|error| panic!("config must prepare: {error}"));
    let video = config.video.as_ref().unwrap_or_else(|| panic!("video must exist"));
    let recording = video.recording.as_ref().unwrap_or_else(|| panic!("recording must exist"));

    assert_eq!(video.extensions, [".ts", ".mp4"]);
    assert_eq!(video.web_search.as_deref(), Some("https://example.test/search"));
    assert_eq!(recording.directory.as_deref(), Some("/var/lib/tuliprox/recordings"));
    assert_eq!(recording.headers.get("Accept").map(String::as_str), Some("video/*"));
    assert!(recording.organize_into_directories);
    assert_eq!(recording.episode_pattern.as_deref(), Some(".*"));
    assert_eq!(recording.priority, 7);

    let serialized = serde_saphyr::to_string(&config).unwrap_or_else(|error| panic!("config must serialize: {error}"));
    assert!(serialized.contains("recording:"));
    assert!(!serialized.contains("download:"));
    assert!(!serialized.contains("download_priority"));
    assert!(!serialized.contains("recording_priority"));

    let round_trip = parse(&serialized);
    let recording = round_trip
        .video
        .as_ref()
        .and_then(|video| video.recording.as_ref())
        .unwrap_or_else(|| panic!("recording must survive serialization"));
    assert_eq!(recording.headers.get("Accept").map(String::as_str), Some("video/*"));
    assert_eq!(recording.directory.as_deref(), Some("/var/lib/tuliprox/recordings"));
    assert_eq!(recording.priority, 7);
}

#[test]
fn recording_round_trips_transfer_and_dvr_fields() {
    let recording = RecordingConfigDto {
        headers: [("X-Upstream-Key".to_string(), "secret".to_string())].into(),
        organize_into_directories: true,
        episode_pattern: Some("(?P<episode>S\\d+E\\d+)".to_string()),
        priority: -2,
        reserve_slots_for_users: 1,
        max_background_per_provider: 2,
        retry_backoff_initial_secs: 4,
        retry_backoff_multiplier: 2.0,
        retry_backoff_max_secs: 60,
        retry_backoff_jitter_percent: 10,
        retry_max_attempts: 6,
        directory: Some("/srv/recordings".to_string()),
        default_pre_roll_secs: Some(30),
        max_pre_roll_secs: 60,
        default_post_roll_secs: Some(45),
        max_post_roll_secs: 90,
        retention: Some(RecordingRetentionConfigDto {
            keep_last_per_channel: Some(5),
            delete_after_days: Some(7),
            sweep_interval_secs: 120,
        }),
        disk: Some(RecordingDiskConfigDto {
            high_water_percent: Some(90),
            low_water_percent: Some(70),
            cleanup_interval_secs: Some(60),
            safety_bytes: Some(1024),
        }),
        quota: Some(RecordingQuotaConfigDto {
            default_private_bytes: Some(100),
            per_user_bytes: [("user".to_string(), 200)].into(),
            shared_bytes: Some(300),
        }),
        notifications: Some(RecordingNotificationConfigDto {
            outbox_buffer: 8,
            max_attempts: 3,
            backoff_initial_secs: 2,
            backoff_max_secs: 20,
        }),
        ..RecordingConfigDto::default()
    };
    let mut config = ConfigDto {
        video: Some(VideoConfigDto {
            extensions: vec!["ts".to_string()],
            web_search: None,
            recording: Some(recording),
        }),
        ..ConfigDto::default()
    };
    config.prepare(false).unwrap_or_else(|error| panic!("config must prepare: {error}"));

    let serialized = serde_saphyr::to_string(&config).unwrap_or_else(|error| panic!("config must serialize: {error}"));
    let round_trip = parse(&serialized);
    assert_eq!(round_trip.video, config.video);
}

#[test]
fn recording_rejects_invalid_padding_retention_disk_and_notifications() {
    let invalid = [
        RecordingConfigDto { default_pre_roll_secs: Some(2), max_pre_roll_secs: 1, ..RecordingConfigDto::default() },
        RecordingConfigDto {
            retention: Some(RecordingRetentionConfigDto { keep_last_per_channel: Some(0), ..Default::default() }),
            ..RecordingConfigDto::default()
        },
        RecordingConfigDto {
            disk: Some(RecordingDiskConfigDto {
                high_water_percent: Some(70),
                low_water_percent: Some(70),
                ..Default::default()
            }),
            ..RecordingConfigDto::default()
        },
        RecordingConfigDto {
            notifications: Some(RecordingNotificationConfigDto { outbox_buffer: 0, ..Default::default() }),
            ..RecordingConfigDto::default()
        },
    ];

    for recording in invalid {
        let mut config = ConfigDto {
            video: Some(VideoConfigDto { extensions: Vec::new(), web_search: None, recording: Some(recording) }),
            ..ConfigDto::default()
        };
        assert!(config.prepare(false).is_err());
    }
}

#[test]
fn removed_recording_shapes_and_priority_aliases_are_rejected() {
    for yaml in [
        "api: { host: 127.0.0.1, port: 8901, web_root: ./web }\nvideo: { download: {} }",
        "api: { host: 127.0.0.1, port: 8901, web_root: ./web }\nrecording: {}",
        "api: { host: 127.0.0.1, port: 8901, web_root: ./web }\nvideo: { recording: { download_priority: 1 } }",
        "api: { host: 127.0.0.1, port: 8901, web_root: ./web }\nvideo: { recording: { recording_priority: 1 } }",
    ] {
        assert!(serde_saphyr::from_str::<ConfigDto>(yaml).is_err(), "obsolete shape accepted: {yaml}");
    }
}
