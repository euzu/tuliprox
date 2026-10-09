use super::*;
use crate::{
    defaults::{
        CONFIG_PATH, DEFAULT_BACKUP_DIR, DEFAULT_STORAGE_DIR, DEFAULT_USER_CONFIG_DIR, MAPPING_FILE, TEMPLATE_FILE,
    },
    model::RecordingConfigDto,
};
use serde_json::json;

#[test]
fn default_uses_connect_timeout_default_value() {
    let cfg = ConfigDto::default();
    assert_eq!(cfg.connect_timeout_secs, default_connect_timeout_secs());
    assert_eq!(cfg.interner_gc_interval_secs, default_interner_gc_interval_secs());
    assert_eq!(cfg.interner_gc_min_pool_size, default_interner_gc_min_pool_size());
}

#[test]
fn custom_video_stream_defaults_are_true_and_502() {
    let cfg = ConfigDto::default();
    assert!(cfg.custom_stream_response_enabled);
    assert_eq!(cfg.custom_stream_response_error_status, 502);
}

#[test]
fn prepare_rejects_non_4xx_5xx_custom_stream_response_error_status() {
    for bad in [200u16, 100, 399, 600, 1000] {
        let mut cfg = ConfigDto { custom_stream_response_error_status: bad, ..ConfigDto::default() };
        let err = cfg.prepare(false).expect_err(&format!("status {bad} must be rejected"));
        let msg = format!("{err}");
        assert!(msg.contains("custom_stream_response_error_status"), "status {bad} msg: {msg}");
    }
}

#[test]
fn prepare_accepts_4xx_and_5xx_custom_stream_response_error_status() {
    for ok in [400u16, 404, 500, 502, 503, 599] {
        let mut cfg = ConfigDto { custom_stream_response_error_status: ok, ..ConfigDto::default() };
        assert!(cfg.prepare(false).is_ok(), "status {ok} must be accepted");
    }
}

#[test]
fn prepare_clamps_zero_custom_stream_response_error_status_to_default() {
    let mut cfg = ConfigDto { custom_stream_response_error_status: 0, ..ConfigDto::default() };
    cfg.prepare(false).expect("zero must be silently clamped, not rejected");
    assert_eq!(cfg.custom_stream_response_error_status, 502);
}

#[test]
fn serializing_skips_default_storage_backup_and_user_config_dirs() {
    let cfg = ConfigDto {
        storage_dir: Some(DEFAULT_STORAGE_DIR.to_string()),
        backup_dir: Some(DEFAULT_BACKUP_DIR.to_string()),
        user_config_dir: Some(DEFAULT_USER_CONFIG_DIR.to_string()),
        ..ConfigDto::default()
    };

    let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
    assert!(
        !serialized.contains("\"storage_dir\""),
        "expected no storage_dir field for default value, got: {serialized}"
    );
    assert!(
        !serialized.contains("\"backup_dir\""),
        "expected no backup_dir field for default value, got: {serialized}"
    );
    assert!(
        !serialized.contains("\"user_config_dir\""),
        "expected no user_config_dir field for default value, got: {serialized}"
    );
}

#[test]
fn serializing_keeps_non_default_storage_and_backup_dirs() {
    let cfg = ConfigDto {
        storage_dir: Some("custom-storage".to_string()),
        backup_dir: Some("custom-backup".to_string()),
        user_config_dir: Some("custom-user-config".to_string()),
        ..ConfigDto::default()
    };

    let serialized = serde_json::to_string(&cfg).expect("config serialization should succeed");
    assert!(
        serialized.contains("\"storage_dir\""),
        "expected storage_dir field for non-default value, got: {serialized}"
    );
    assert!(
        serialized.contains("\"backup_dir\""),
        "expected backup_dir field for non-default value, got: {serialized}"
    );
    assert!(
        serialized.contains("\"user_config_dir\""),
        "expected user_config_dir field for non-default value, got: {serialized}"
    );
}

#[test]
fn main_config_from_applies_default_storage_backup_and_user_config_dirs() {
    let mut cfg = ConfigDto::default();
    cfg.prepare(false).expect("prepare should succeed");
    let main = MainConfigDto::from(&cfg);
    assert_eq!(main.storage_dir.as_deref(), Some(DEFAULT_STORAGE_DIR));
    assert_eq!(main.backup_dir.as_deref(), Some(DEFAULT_BACKUP_DIR));
    assert_eq!(main.user_config_dir.as_deref(), Some(DEFAULT_USER_CONFIG_DIR));
    assert_eq!(main.mapping_path.as_deref(), Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}").as_str()));
    assert_eq!(main.template_path.as_deref(), Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}").as_str()));
}

#[test]
fn update_from_main_config_omits_default_optional_paths() {
    let mut cfg = ConfigDto::default();
    let main = MainConfigDto {
        storage_dir: Some(DEFAULT_STORAGE_DIR.to_string()),
        backup_dir: Some(DEFAULT_BACKUP_DIR.to_string()),
        user_config_dir: Some(DEFAULT_USER_CONFIG_DIR.to_string()),
        mapping_path: Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}")),
        template_path: Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}")),
        ..MainConfigDto::default()
    };

    cfg.update_from_main_config(&main);
    assert!(cfg.storage_dir.is_none());
    assert!(cfg.backup_dir.is_none());
    assert!(cfg.user_config_dir.is_none());
    assert!(cfg.mapping_path.is_none());
    assert!(cfg.template_path.is_none());
}

#[test]
fn prepare_sets_default_optional_paths() {
    let mut cfg = ConfigDto {
        storage_dir: None,
        backup_dir: None,
        user_config_dir: None,
        mapping_path: None,
        template_path: None,
        ..ConfigDto::default()
    };
    cfg.prepare(false).expect("prepare should succeed");
    assert_eq!(cfg.storage_dir.as_deref(), Some(DEFAULT_STORAGE_DIR));
    assert_eq!(cfg.backup_dir.as_deref(), Some(DEFAULT_BACKUP_DIR));
    assert_eq!(cfg.user_config_dir.as_deref(), Some(DEFAULT_USER_CONFIG_DIR));
    assert_eq!(cfg.mapping_path.as_deref(), Some(format!("./{CONFIG_PATH}/{MAPPING_FILE}").as_str()));
    assert_eq!(cfg.template_path.as_deref(), Some(format!("./{CONFIG_PATH}/{TEMPLATE_FILE}").as_str()));
}

#[test]
fn deserializing_rejects_legacy_video_ffprobe_fields() {
    let raw = json!({
        "api": {
            "host": "127.0.0.1",
            "port": 8901,
            "web_root": "./web"
        },
        "storage_dir": ".",
        "video": {
            "extensions": ["mp4"],
            "ffprobe_enabled": true
        }
    });

    let result: Result<ConfigDto, _> = serde_json::from_value(raw);
    assert!(result.is_err(), "legacy ffprobe field under video must fail");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("ffprobe_enabled"), "unexpected error text: {err}");
}

#[test]
fn deserializing_rejects_legacy_data_dir_alias() {
    let raw = json!({
        "api": {
            "host": "127.0.0.1",
            "port": 8901,
            "web_root": "./web"
        },
        "data_dir": "."
    });

    let result: Result<ConfigDto, _> = serde_json::from_value(raw);
    assert!(result.is_err(), "data_dir should not deserialize");
}

#[test]
fn deserializing_accepts_legacy_working_dir_alias() {
    let raw = json!({
        "api": {
            "host": "127.0.0.1",
            "port": 8901,
            "web_root": "./web"
        },
        "working_dir": "."
    });

    let cfg: ConfigDto = serde_json::from_value(raw).expect("working_dir should deserialize as legacy alias");
    assert_eq!(cfg.storage_dir.as_deref(), Some("."));
}

#[test]
fn deserializing_accepts_missing_storage_dir() {
    let raw = json!({
        "api": {
            "host": "127.0.0.1",
            "port": 8901,
            "web_root": "./web"
        }
    });

    let cfg: ConfigDto = serde_json::from_value(raw).expect("missing storage_dir should deserialize");
    assert!(cfg.storage_dir.is_none());
}

#[test]
fn stream_history_defaults_to_disabled_and_safe() {
    let cfg = ConfigDto::default();

    assert!(cfg.reverse_proxy.is_none());
}

#[test]
fn stream_history_deserializes_under_reverse_proxy() {
    let raw = r"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
reverse_proxy:
  rewrite_secret: 00112233445566778899aabbccddeeff
  stream_history:
    stream_history_enabled: true
    stream_history_batch_size: 64
    stream_history_retention_days: 14
    stream_history_directory: /var/lib/tuliprox/history
";

    let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config should deserialize");
    let reverse_proxy = cfg.reverse_proxy.expect("reverse_proxy should deserialize");
    let stream_history = reverse_proxy.stream_history.expect("stream_history should deserialize");

    assert!(stream_history.stream_history_enabled);
    assert_eq!(stream_history.stream_history_batch_size, 64);
    assert_eq!(stream_history.stream_history_retention_days, 14);
    assert_eq!(stream_history.stream_history_directory, "/var/lib/tuliprox/history");
}

#[test]
fn stream_history_missing_values_keep_disabled_without_reverse_proxy() {
    let raw = r"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
";

    let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config should deserialize");

    assert!(cfg.reverse_proxy.is_none());
}

#[test]
fn stream_history_defaults_to_none_when_reverse_proxy_present_but_stream_history_omitted() {
    let raw = r#"
api:
  host: 127.0.0.1
  port: 8901
  web_root: ./web
reverse_proxy:
  resource_rewrite_disabled: false
  rewrite_secret: "00112233445566778899aabbccddeeff"
"#;

    let cfg: ConfigDto = serde_saphyr::from_str(raw).expect("config should deserialize");

    assert!(cfg.reverse_proxy.is_some());
    assert!(cfg.reverse_proxy.as_ref().and_then(|rp| rp.stream_history.as_ref()).is_none());
}

#[test]
fn is_stream_history_and_qos_enabled_match_nested_config() {
    let mut cfg = ConfigDto::default();
    assert!(!cfg.is_stream_history_enabled());
    assert!(!cfg.is_qos_aggregation_enabled());

    let rp = ReverseProxyConfigDto {
        stream_history: Some(crate::model::StreamHistoryConfigDto {
            stream_history_enabled: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    cfg.reverse_proxy = Some(rp);
    assert!(cfg.is_stream_history_enabled());
    assert!(!cfg.is_qos_aggregation_enabled());

    cfg.reverse_proxy.as_mut().unwrap().qos_aggregation =
        Some(crate::model::QosAggregationConfigDto { enabled: true, ..Default::default() });
    assert!(cfg.is_stream_history_enabled());
    assert!(cfg.is_qos_aggregation_enabled());

    // Disabling stream history disables QoS aggregation as well
    cfg.reverse_proxy.as_mut().unwrap().stream_history.as_mut().unwrap().stream_history_enabled = false;
    assert!(!cfg.is_stream_history_enabled());
    assert!(!cfg.is_qos_aggregation_enabled());
}

#[test]
fn is_recording_enabled_matches_nested_config() {
    let mut cfg = ConfigDto::default();
    assert!(!cfg.is_recording_enabled());

    let video = VideoConfigDto {
        recording: Some(RecordingConfigDto { enabled: true, ..Default::default() }),
        ..Default::default()
    };
    cfg.video = Some(video);
    assert!(cfg.is_recording_enabled());

    cfg.video.as_mut().unwrap().recording.as_mut().unwrap().enabled = false;
    assert!(!cfg.is_recording_enabled());
}
