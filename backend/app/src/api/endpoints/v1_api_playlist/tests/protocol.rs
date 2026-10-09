use crate::model::{Config, ConfigInput, RecordingConfig};
use std::{collections::HashMap, sync::Arc};

#[tokio::test]
async fn recording_headers_layer_under_input_headers_without_touching_the_worker_request() {
    let mut recording = RecordingConfig::from(&shared::model::RecordingConfigDto::default());
    recording.headers = HashMap::from([
        ("User-Agent".to_string(), "VLC/3.0.16 LibVLC/3.0.16".to_string()),
        ("Accept".to_string(), "video/*".to_string()),
        ("X-Recording-Test".to_string(), "custom".to_string()),
        ("invalid header".to_string(), "ignored".to_string()),
    ]);
    let state = crate::api::model::create_test_app_state(Config {
        video: Some(tuliprox_core::model::VideoConfig {
            extensions: vec![],
            web_search: None,
            recording: Some(recording),
        }),
        ..Default::default()
    });
    let header = |input: &ConfigInput, name: &str| {
        input.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.clone())
    };

    let plain = Arc::new(ConfigInput { name: "input".into(), ..Default::default() });
    let merged = crate::api::api_utils::with_recording_headers(&state.app_config, Arc::clone(&plain));
    assert_eq!(header(&merged, "user-agent").as_deref(), Some("VLC/3.0.16 LibVLC/3.0.16"));
    assert_eq!(header(&merged, "accept").as_deref(), Some("video/*"));
    assert_eq!(header(&merged, "x-recording-test").as_deref(), Some("custom"));
    assert_eq!(header(&merged, "invalid header"), None);
    assert_eq!(merged.name, plain.name);
    assert!(plain.headers.is_empty(), "the configured input must stay untouched");

    let with_agent = Arc::new(ConfigInput {
        name: "input".into(),
        headers: HashMap::from([("user-agent".to_string(), "input-agent".to_string())]),
        ..Default::default()
    });
    let merged = crate::api::api_utils::with_recording_headers(&state.app_config, with_agent);
    assert_eq!(header(&merged, "user-agent").as_deref(), Some("input-agent"));
    assert_eq!(merged.headers.keys().filter(|key| key.eq_ignore_ascii_case("user-agent")).count(), 1);
    assert_eq!(header(&merged, "accept").as_deref(), Some("video/*"));
}
