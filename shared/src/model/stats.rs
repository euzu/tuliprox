use crate::model::InputType;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt::Display;

pub fn format_elapsed_time(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds} secs")
    } else {
        let minutes = seconds / 60;
        let seconds = seconds % 60;
        format!("{minutes}:{seconds:02} mins")
    }
}

fn serialize_elapsed_time<S>(secs: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let formatted = format_elapsed_time(*secs);
    serializer.serialize_str(&formatted)
}

fn deserialize_elapsed_time<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.ends_with(" secs") {
        s.trim_end_matches(" secs").parse::<u64>().map_err(serde::de::Error::custom)
    } else if s.ends_with(" mins") {
        let parts: Vec<&str> = s.trim_end_matches(" mins").split(':').collect();
        match parts.as_slice() {
            [mins, secs] => {
                let mins = mins.parse::<u64>().map_err(serde::de::Error::custom)?;
                let secs = secs.parse::<u64>().map_err(serde::de::Error::custom)?;
                mins.checked_mul(60)
                    .and_then(|m| m.checked_add(secs))
                    .ok_or_else(|| serde::de::Error::custom("elapsed time overflow"))
            }
            [mins] => {
                // Fallback if no colon (e.g. just "5 mins")
                let mins = mins.parse::<u64>().map_err(serde::de::Error::custom)?;
                mins.checked_mul(60).ok_or_else(|| serde::de::Error::custom("elapsed time overflow"))
            }
            _ => Err(serde::de::Error::custom(format!("invalid elapsed time: {s}"))),
        }
    } else {
        s.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistStats {
    #[serde(rename = "groups")]
    pub group_count: usize,
    #[serde(rename = "channels")]
    pub channel_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputStats {
    pub name: String,
    #[serde(rename = "type")]
    pub input_type: InputType,
    #[serde(rename = "errors")]
    pub error_count: usize,
    #[serde(rename = "raw")]
    pub raw_stats: PlaylistStats,
    #[serde(rename = "processed")]
    pub processed_stats: PlaylistStats,
    #[serde(rename = "took", serialize_with = "serialize_elapsed_time", deserialize_with = "deserialize_elapsed_time")]
    pub secs_took: u64,
}

impl Display for InputStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_string(&self).map_or(Err(std::fmt::Error), |json_str| write!(f, "{json_str}"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineStats {
    pub inspected: usize,
    pub retained: usize,
    pub removed: usize,
    pub renamed_items: usize,
    pub renamed_fields: usize,
    pub matched_mapping_rules: usize,
    pub emitted_items: usize,
    pub mapping_diagnostics: usize,
}

impl PipelineStats {
    pub const fn is_empty(&self) -> bool {
        self.inspected == 0
            && self.retained == 0
            && self.removed == 0
            && self.renamed_items == 0
            && self.renamed_fields == 0
            && self.matched_mapping_rules == 0
            && self.emitted_items == 0
            && self.mapping_diagnostics == 0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetStats {
    #[serde(rename = "target")]
    pub name: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processing: Option<PipelineStats>,
}

impl TargetStats {
    pub fn success(name: &str) -> Self { Self { name: name.to_string(), success: true, processing: None } }
    pub fn failure(name: &str) -> Self { Self { name: name.to_string(), success: false, processing: None } }

    pub fn success_with_processing(name: &str, processing: PipelineStats) -> Self {
        Self { name: name.to_string(), success: true, processing: (!processing.is_empty()).then_some(processing) }
    }

    pub fn failure_with_processing(name: &str, processing: PipelineStats) -> Self {
        Self { name: name.to_string(), success: false, processing: (!processing.is_empty()).then_some(processing) }
    }
}

impl Display for TargetStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_string(&self).map_or(Err(std::fmt::Error), |json_str| write!(f, "{json_str}"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStats {
    #[serde(rename = "inputs")]
    pub inputs: Vec<InputStats>,
    #[serde(rename = "targets")]
    pub targets: Vec<TargetStats>,
}

impl SourceStats {
    pub fn try_new(inputs: Vec<InputStats>, targets: Vec<TargetStats>) -> Option<Self> {
        if inputs.is_empty() || targets.is_empty() {
            None
        } else {
            Some(Self { inputs, targets })
        }
    }
}

impl Display for SourceStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_string(&self).map_or(Err(std::fmt::Error), |json_str| write!(f, "{json_str}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_stats_accept_legacy_json_without_processing() {
        let stats: TargetStats =
            serde_json::from_str(r#"{"target":"default","success":true}"#).expect("legacy stats should deserialize");

        assert_eq!(stats, TargetStats::success("default"));
    }

    #[test]
    fn empty_processing_stats_are_omitted() {
        let stats = TargetStats::success_with_processing("default", PipelineStats::default());
        let json = serde_json::to_value(stats).expect("stats should serialize");

        assert!(json.get("processing").is_none());
    }
}
