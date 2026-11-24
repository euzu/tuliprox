use crate::model::PlayerRecordingLevel;
use serde::{Deserialize, Serialize};

/// Effective capabilities and limits of a playlist proxy user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerCapabilitiesDto {
    pub source_switch: bool,
    pub joinable_streams: bool,
    pub recording: PlayerRecordingLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_quota_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_recordings: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_recording_rules: Option<u32>,
    pub free_shared_joins: bool,
    pub max_free_shared_joins: u32,
    pub watch_progress: bool,
    pub event_stream: bool,
    pub stream_split: bool,
    pub effective_permissions: Vec<String>,
    pub max_connections: u32,
    pub active_connections: u32,
    pub shared_joins: u32,
    pub free_shared_joins_in_use: u32,
    pub share_live_streams: bool,
    pub source_switch_protocols: Vec<String>,
    pub recording_enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_capabilities_dto_serde_roundtrip() {
        let dto = PlayerCapabilitiesDto {
            source_switch: true,
            joinable_streams: true,
            recording: PlayerRecordingLevel::Private,
            recording_quota_bytes: Some(1024),
            max_concurrent_recordings: Some(2),
            max_recording_rules: Some(10),
            free_shared_joins: true,
            max_free_shared_joins: 3,
            watch_progress: true,
            event_stream: false,
            stream_split: true,
            effective_permissions: vec!["recording.read".to_string()],
            max_connections: 2,
            active_connections: 1,
            shared_joins: 0,
            free_shared_joins_in_use: 0,
            share_live_streams: true,
            source_switch_protocols: vec!["ts".to_string(), "hls".to_string()],
            recording_enabled: true,
        };
        let json = serde_json::to_string(&dto).expect("serialize should succeed");
        assert!(json.contains("\"stream_split\":true"));
        assert!(json.contains("\"max_recording_rules\":10"));
        assert!(json.contains("\"watch_progress\":true"));
        assert!(json.contains("\"event_stream\":false"));
        let deserialized: PlayerCapabilitiesDto = serde_json::from_str(&json).expect("deserialize should succeed");
        assert_eq!(dto, deserialized);
    }
}
