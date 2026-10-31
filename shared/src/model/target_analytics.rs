use crate::model::{EpgSourceTypeDto, TargetType};
use serde::{Deserialize, Serialize};

pub const TARGET_ANALYTICS_VERSION: u8 = 1;

const fn default_analytics_version() -> u8 { TARGET_ANALYTICS_VERSION }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClusterAnalytics {
    pub count: usize,
    pub groups: usize,
    pub input_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GroupCount {
    pub name: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpgSourceAnalytics {
    pub input: String,
    pub label: String,
    pub kind: EpgSourceTypeDto,
    #[serde(default)]
    pub mapped_channels: usize,
    #[serde(default)]
    pub guide_days: Option<u16>,
    #[serde(default)]
    pub fetched_at: Option<u64>,
    #[serde(default)]
    pub failed_at: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EpgAnalytics {
    pub live_channels: usize,
    pub mapped: usize,
    #[serde(default)]
    pub guide_until: Option<i64>,
    #[serde(default)]
    pub sources: Vec<EpgSourceAnalytics>,
    #[serde(default)]
    pub missing_by_group: Vec<GroupCount>,
    #[serde(default)]
    pub missing_other: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetAnalyticsDto {
    #[serde(default = "default_analytics_version")]
    pub version: u8,
    pub success: bool,
    pub updated_at: u64,
    pub took_secs: u64,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub live: ClusterAnalytics,
    pub vod: ClusterAnalytics,
    pub series: ClusterAnalytics,
    #[serde(default)]
    pub episodes: Option<usize>,
    /// False when only part of the series were resolved; `episodes` is then a lower bound.
    #[serde(default)]
    pub episodes_complete: bool,
    #[serde(default)]
    pub catchup_channels: usize,
    #[serde(default)]
    pub catchup_max_days: Option<u32>,
    #[serde(default)]
    pub top_live_groups: Vec<GroupCount>,
    #[serde(default)]
    pub epg: Option<EpgAnalytics>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetAnalyticsEntryDto {
    pub target_id: u16,
    pub name: String,
    #[serde(default)]
    pub outputs: Vec<TargetType>,
    #[serde(default)]
    pub next_scheduled: Option<i64>,
    #[serde(default)]
    pub analytics: Option<TargetAnalyticsDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TargetAnalyticsResponseDto {
    #[serde(default)]
    pub targets: Vec<TargetAnalyticsEntryDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_analytics_dto_serde_round_trip() {
        let dto = TargetAnalyticsDto {
            version: TARGET_ANALYTICS_VERSION,
            success: true,
            updated_at: 1_700_000_000,
            took_secs: 5,
            inputs: vec!["provider1".to_string(), "provider2".to_string()],
            live: ClusterAnalytics { count: 120, groups: 12, input_count: 150 },
            vod: ClusterAnalytics { count: 450, groups: 30, input_count: 500 },
            series: ClusterAnalytics { count: 80, groups: 8, input_count: 90 },
            episodes: Some(1200),
            episodes_complete: true,
            catchup_channels: 25,
            catchup_max_days: Some(7),
            top_live_groups: vec![
                GroupCount { name: "Sports".to_string(), count: 50 },
                GroupCount { name: "News".to_string(), count: 30 },
            ],
            epg: Some(EpgAnalytics {
                live_channels: 120,
                mapped: 110,
                guide_until: Some(1_700_604_800),
                sources: vec![EpgSourceAnalytics {
                    input: "provider1".to_string(),
                    label: "guide.xml".to_string(),
                    kind: EpgSourceTypeDto::Xmltv,
                    mapped_channels: 110,
                    guide_days: Some(7),
                    fetched_at: Some(1_699_990_000),
                    failed_at: None,
                    error: None,
                }],
                missing_by_group: vec![GroupCount { name: "Local".to_string(), count: 10 }],
                missing_other: 0,
            }),
        };

        let json = serde_json::to_string_pretty(&dto).expect("serialize TargetAnalyticsDto");
        let decoded: TargetAnalyticsDto = serde_json::from_str(&json).expect("deserialize TargetAnalyticsDto");
        assert_eq!(decoded, dto);

        let response = TargetAnalyticsResponseDto {
            targets: vec![TargetAnalyticsEntryDto {
                target_id: 1,
                name: "Main Target".to_string(),
                outputs: vec![TargetType::M3u, TargetType::Xtream],
                next_scheduled: Some(1_700_003_600),
                analytics: Some(dto),
            }],
        };

        let resp_json = serde_json::to_string(&response).expect("serialize TargetAnalyticsResponseDto");
        let resp_decoded: TargetAnalyticsResponseDto =
            serde_json::from_str(&resp_json).expect("deserialize TargetAnalyticsResponseDto");
        assert_eq!(resp_decoded, response);
    }

    #[test]
    fn test_legacy_default_deserialization() {
        let minimal_json = r#"{
            "success": true,
            "updated_at": 1700000000,
            "took_secs": 2,
            "live": {"count": 10, "groups": 2, "input_count": 10},
            "vod": {"count": 0, "groups": 0, "input_count": 0},
            "series": {"count": 0, "groups": 0, "input_count": 0}
        }"#;

        let decoded: TargetAnalyticsDto = serde_json::from_str(minimal_json).expect("deserialize minimal analytics");
        assert_eq!(decoded.version, TARGET_ANALYTICS_VERSION);
        assert!(decoded.success);
        assert_eq!(decoded.inputs, Vec::<String>::new());
        assert_eq!(decoded.episodes, None);
        assert!(!decoded.episodes_complete);
        assert_eq!(decoded.catchup_channels, 0);
        assert_eq!(decoded.catchup_max_days, None);
        assert_eq!(decoded.top_live_groups, Vec::<GroupCount>::new());
        assert_eq!(decoded.epg, None);

        let minimal_entry = r#"{
            "target_id": 5,
            "name": "Target 5"
        }"#;
        let entry_decoded: TargetAnalyticsEntryDto =
            serde_json::from_str(minimal_entry).expect("deserialize minimal entry");
        assert_eq!(entry_decoded.target_id, 5);
        assert_eq!(entry_decoded.name, "Target 5");
        assert_eq!(entry_decoded.outputs, Vec::<TargetType>::new());
        assert_eq!(entry_decoded.next_scheduled, None);
        assert_eq!(entry_decoded.analytics, None);
    }
}
