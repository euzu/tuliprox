use super::MetadataUpdateConfigDto;
use crate::model::ByteSize;

#[test]
fn default_config_is_empty() {
    let cfg = MetadataUpdateConfigDto::default();
    assert!(cfg.is_empty());
}

#[test]
fn prepare_keeps_default_config_empty() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.prepare().expect("metadata update config defaults should be valid");
    assert!(cfg.is_empty());
}

#[test]
fn prepare_parses_duration_suffixes() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.log.queue_interval = "1m".to_string();
    cfg.log.progress_interval = "2h".to_string();
    cfg.probe.cooldown = "1d".to_string();
    cfg.tmdb.cooldown = "2d".to_string();

    cfg.prepare().expect("metadata update config should parse duration values");

    assert_eq!(cfg.log.queue_interval, "1m");
    assert_eq!(cfg.log.progress_interval, "2h");
    assert_eq!(cfg.probe.cooldown, "1d");
    assert_eq!(cfg.tmdb.cooldown, "2d");
}

#[test]
fn prepare_clamps_minimum_values() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.log.queue_interval = "0".to_string();
    cfg.resolve.max_attempts = 0;
    cfg.probe.max_attempts = 0;
    cfg.max_queue_size = 0;
    cfg.no_change_cache_ttl_secs = 0;
    cfg.probe_fairness_resolve_burst = 0;
    cfg.ffprobe.timeout = Some(0);
    cfg.ffprobe.analyze_duration = "0s".to_string();
    cfg.ffprobe.probe_size = ByteSize::new("0");

    cfg.prepare().expect("metadata update config should clamp minimum values");

    assert_eq!(cfg.log.queue_interval, "1s");
    assert_eq!(cfg.resolve.max_attempts, 1);
    assert_eq!(cfg.probe.max_attempts, 1);
    assert_eq!(cfg.max_queue_size, 1);
    assert_eq!(cfg.no_change_cache_ttl_secs, 1);
    assert_eq!(cfg.probe_fairness_resolve_burst, 1);
    assert_eq!(cfg.ffprobe.timeout, Some(1));
    assert_eq!(cfg.ffprobe.analyze_duration, "1s");
    assert_eq!(cfg.ffprobe.probe_size, ByteSize::new("1B"));
}

#[test]
fn prepare_rejects_invalid_duration_unit() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.log.queue_interval = "1w".to_string();

    let result = cfg.prepare();
    assert!(result.is_err(), "invalid duration unit must fail");
}

#[test]
fn prepare_canonicalizes_to_larger_units() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.probe.cooldown = "604800".to_string();
    cfg.tmdb.cooldown = "259200".to_string();
    cfg.worker_idle_timeout = "60".to_string();
    cfg.probe.retry_backoff_step_3 = "3600".to_string();
    cfg.ffprobe.analyze_duration = "10s".to_string();
    cfg.ffprobe.probe_size = ByteSize::new("10485760");
    cfg.ffprobe.live_analyze_duration = "5s".to_string();
    cfg.ffprobe.live_probe_size = ByteSize::new("5242880");

    cfg.prepare().expect("metadata update config should canonicalize durations");

    assert_eq!(cfg.probe.cooldown, "7d");
    assert_eq!(cfg.tmdb.cooldown, "3d");
    assert_eq!(cfg.worker_idle_timeout, "1m");
    assert_eq!(cfg.probe.retry_backoff_step_3, "1h");
    assert_eq!(cfg.ffprobe.analyze_duration, "10s");
    assert_eq!(cfg.ffprobe.probe_size, ByteSize::new("10MB"));
    assert_eq!(cfg.ffprobe.live_analyze_duration, "5s");
    assert_eq!(cfg.ffprobe.live_probe_size, ByteSize::new("5MB"));
}

#[test]
fn prepare_rejects_ffprobe_duration_without_unit() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.ffprobe.analyze_duration = "10000000".to_string();

    let result = cfg.prepare();
    assert!(result.is_err(), "numeric ffprobe analyze duration without unit must fail");
    let err_text = result.expect_err("validation should fail").to_string();
    assert!(err_text.contains("ffprobe.analyze_duration"));
}

#[test]
fn tmdb_non_default_match_threshold_is_not_empty() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.tmdb.match_threshold = 90;
    cfg.prepare().expect("metadata update config should remain valid");

    assert!(!cfg.tmdb.is_empty(), "tmdb config with non-default match threshold must not be empty");
    assert!(!cfg.is_empty(), "metadata update config with non-default tmdb match threshold must not be empty");
}

#[test]
fn prepare_clamps_tmdb_match_threshold() {
    let mut cfg = MetadataUpdateConfigDto::default();
    cfg.tmdb.match_threshold = 250;
    cfg.prepare().expect("metadata update config should clamp tmdb match threshold");

    assert_eq!(cfg.tmdb.match_threshold, 100);
}
