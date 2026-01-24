use std::fmt::Display;
use serde::{Deserialize, Serialize, Serializer, Deserializer};
use crate::model::InputType;

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
        if parts.len() == 2 {
            let mins = parts[0].parse::<u64>().map_err(serde::de::Error::custom)?;
            let secs = parts[1].parse::<u64>().map_err(serde::de::Error::custom)?;
            Ok(mins * 60 + secs)
        } else {
             // Fallback if no colon (e.g. just "5 mins")
             parts[0].parse::<u64>().map(|m| m * 60).map_err(serde::de::Error::custom)
        }
    } else {
        s.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistStats {
    #[serde(rename = "groups")]
    pub group_count: usize,
    #[serde(rename = "channels")]
    pub channel_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetStats {
    #[serde(rename = "target")]
    pub name: String,
    pub success: bool,
}

impl TargetStats {
    pub fn success(name: &str) -> Self {
        Self  {name: name.to_string(), success: true}
    }
    pub fn failure(name: &str) -> Self {
        Self  {name: name.to_string(), success: false}
    }
}

impl Display for TargetStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_string(&self).map_or(Err(std::fmt::Error), |json_str| write!(f, "{json_str}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            Some(Self {inputs, targets})
        }
    }
}

impl Display for SourceStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        serde_json::to_string(&self).map_or(Err(std::fmt::Error), |json_str| write!(f, "{json_str}"))
    }
}

