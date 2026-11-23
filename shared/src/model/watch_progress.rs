use serde::{Deserialize, Serialize};

/// Media kind tracked by the watch progress API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchProgressKind {
    Movie,
    Episode,
    Recording,
}

/// Request payload for creating or updating playback progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchProgressUpdateDto {
    pub position_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watched: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_id: Option<u32>,
}

/// Representation of a playback progress record returned by the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchProgressDto {
    pub kind: WatchProgressKind,
    pub item_id: u32,
    pub position_secs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    pub watched: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_id: Option<u32>,
    pub updated_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_progress_kind_serde_round_trip() {
        for (kind, expected) in [
            (WatchProgressKind::Movie, "\"movie\""),
            (WatchProgressKind::Episode, "\"episode\""),
            (WatchProgressKind::Recording, "\"recording\""),
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, expected);
            let parsed: WatchProgressKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, kind);
        }
        assert!(serde_json::from_str::<WatchProgressKind>("\"unknown\"").is_err());
    }

    #[test]
    fn watch_progress_dto_serialization_round_trip() {
        let dto = WatchProgressDto {
            kind: WatchProgressKind::Episode,
            item_id: 42,
            position_secs: 1500,
            duration_secs: Some(3000),
            watched: false,
            series_id: Some(10),
            updated_at: 1_700_000_000,
        };
        let serialized = serde_json::to_string(&dto).unwrap();
        let deserialized: WatchProgressDto = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, dto);
    }
}
