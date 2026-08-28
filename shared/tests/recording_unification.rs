//! Integration tests for the recording-unification schema lift.
//! The legacy fixture under `tests/fixtures/` is the literal pre-upgrade
//! configuration an operator was running; we parse it via `serde_saphyr`,
//! run `prepare()`, and assert that:
//!  * the compat `video.download.recording` shadow is preserved
//!    (frontend still reads it)
//!  * the canonical top-level `recording:` field is populated with
//!    the migrated DVR settings (no fields lost)
//!  * canonical serialization re-emits only `recording:`, never the
//!    legacy `video:` key
//!  * `recording.directory` wins over the `<download.directory>/recordings`
//!    default.

use shared::model::ConfigDto;
use std::fs;

const FIXTURE_PATH: &str = "tests/fixtures/legacy_video_download.yml";

#[test]
fn legacy_fixture_loads_into_canonical_recording_without_losing_fields() {
    let raw = fs::read_to_string(FIXTURE_PATH).expect("legacy fixture must exist");
    let mut cfg: ConfigDto = serde_saphyr::from_str(&raw).expect("legacy YAML must parse");

    // Compat shadow: legacy path still readable by frontend.
    let legacy = cfg
        .video
        .as_ref()
        .and_then(|v| v.download.as_ref())
        .and_then(|d| d.recording.as_ref())
        .expect("legacy video.download.recording must round-trip");
    assert!(legacy.enabled);
    assert_eq!(legacy.directory.as_deref(), Some("/var/lib/tuliprox/recordings"));
    assert_eq!(legacy.timezone.as_deref(), Some("Europe/Berlin"));
    assert_eq!(legacy.filename_template.as_deref(), Some("{channel}_{program_title}_{start_time}"));
    assert_eq!(legacy.default_pre_roll_secs, Some(0));
    assert_eq!(legacy.max_pre_roll_secs, 900);
    assert_eq!(legacy.default_post_roll_secs, Some(0));
    assert_eq!(legacy.max_post_roll_secs, 1800);
    assert!(legacy.retention.is_some());
    assert!(legacy.disk.is_some());
    assert!(legacy.notifications.is_some());

    cfg.prepare(false).expect("prepare should succeed");

    // Canonical top-level field carries the migrated values.
    let canonical = cfg.recording.as_ref().expect("canonical recording must be populated");
    assert!(canonical.enabled);
    assert_eq!(canonical.directory.as_deref(), Some("/var/lib/tuliprox/recordings"));
    assert_eq!(canonical.timezone.as_deref(), Some("Europe/Berlin"));
    assert_eq!(canonical.filename_template.as_deref(), Some("{channel}_{program_title}_{start_time}"));
    assert_eq!(canonical.default_pre_roll_secs, Some(0));
    assert_eq!(canonical.max_pre_roll_secs, 900);
    assert_eq!(canonical.default_post_roll_secs, Some(0));
    assert_eq!(canonical.max_post_roll_secs, 1800);
    assert!(canonical.retention.is_some());
    assert!(canonical.disk.is_some());
    assert!(canonical.notifications.is_some());
    assert_eq!(canonical.headers.get("Accept").map(String::as_str), Some("video/*"));
    assert_eq!(canonical.headers.get("User-Agent").map(String::as_str), Some("legacy-agent"));
    assert_eq!(canonical.extensions, [".ts", ".mp4"]);
    assert!(canonical.organize_into_directories);
    assert_eq!(canonical.episode_pattern.as_deref(), Some(".*"));
    assert_eq!(canonical.priority, 7);
    assert_eq!(canonical.reserve_slots_for_users, 1);
    assert_eq!(canonical.max_background_per_provider, 2);
    assert_eq!(canonical.retry_backoff_initial_secs, 3);
    assert_eq!(canonical.retry_backoff_multiplier, 3.0);
    assert_eq!(canonical.retry_backoff_max_secs, 30);
    assert_eq!(canonical.retry_backoff_jitter_percent, 20);
    assert_eq!(canonical.retry_max_attempts, 5);

    // Canonical serialization never re-emits the compat shadow.
    let serialized = serde_json::to_string(&cfg).expect("serialize");
    assert!(!serialized.contains("\"video\""), "video compat shadow must not be re-emitted, got: {serialized}");
    assert!(serialized.contains("\"recording\""), "recording must be present, got: {serialized}");
    assert!(serialized.contains("\"headers\""), "legacy headers must migrate: {serialized}");
    assert!(serialized.contains("legacy-agent"), "legacy header values must migrate: {serialized}");
    assert!(serialized.contains("\"extensions\""), "legacy extensions must migrate: {serialized}");
    assert!(serialized.contains("\"organize_into_directories\":true"), "organize flag must migrate: {serialized}");
    assert!(serialized.contains("\"episode_pattern\":\".*\""), "episode pattern must migrate: {serialized}");
    assert!(serialized.contains("\"priority\":7"), "recording priority must migrate: {serialized}");
    assert!(!serialized.contains("download_priority"), "obsolete download priority must be discarded: {serialized}");
    assert!(serialized.contains("\"reserve_slots_for_users\":1"), "queue policy must migrate: {serialized}");
    assert!(serialized.contains("\"max_background_per_provider\":2"), "provider limit must migrate: {serialized}");
}
