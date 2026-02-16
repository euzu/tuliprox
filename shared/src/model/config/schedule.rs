#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
pub enum ScheduleTaskType {
    #[default]
    PlaylistUpdate,
    LibraryScan,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfigDto {
    #[serde(default)]
    pub schedule: String,
    #[serde(default, rename = "type")]
    pub task_type: ScheduleTaskType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
}
