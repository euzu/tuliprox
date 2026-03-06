#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
pub enum ScheduleTaskType {
    #[default]
    PlaylistUpdate,
    LibraryScan,
    GeoIpUpdate,
}

const fn default_schedule_task_type() -> ScheduleTaskType { ScheduleTaskType::PlaylistUpdate }

fn parse_schedule_task_type(value: &str) -> Option<ScheduleTaskType> {
    match value.trim().to_ascii_lowercase().as_str() {
        "playlistupdate" | "playlist_update" | "playlist-update" | "update" => Some(ScheduleTaskType::PlaylistUpdate),
        "libraryscan" | "library_scan" | "library-scan" | "scan" => Some(ScheduleTaskType::LibraryScan),
        "geoipupdate" | "geo_ip_update" | "geoip_update" | "geoip-update" | "geoip" => {
            Some(ScheduleTaskType::GeoIpUpdate)
        }
        _ => None,
    }
}

fn deserialize_schedule_task_type<'de, D>(deserializer: D) -> Result<ScheduleTaskType, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum CompatTaskType {
        Enum(ScheduleTaskType),
        String(String),
    }

    let value = Option::<CompatTaskType>::deserialize(deserializer)?;
    match value {
        None => Ok(default_schedule_task_type()),
        Some(CompatTaskType::Enum(task_type)) => Ok(task_type),
        Some(CompatTaskType::String(raw)) => {
            if raw.trim().is_empty() {
                Ok(default_schedule_task_type())
            } else if let Some(task_type) = parse_schedule_task_type(&raw) {
                Ok(task_type)
            } else {
                Err(serde::de::Error::custom(format!(
                    "invalid schedule type '{raw}', expected one of: PlaylistUpdate, LibraryScan, GeoIpUpdate"
                )))
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleConfigDto {
    #[serde(default)]
    pub schedule: String,
    #[serde(
        default = "default_schedule_task_type",
        rename = "type",
        alias = "task_type",
        deserialize_with = "deserialize_schedule_task_type"
    )]
    pub task_type: ScheduleTaskType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::{ScheduleConfigDto, ScheduleTaskType};

    #[test]
    fn defaults_to_playlist_update_when_type_is_missing() {
        let dto: ScheduleConfigDto = serde_saphyr::from_str("schedule: \"0 0 * * * * *\"\ntargets: [\"main\"]")
            .expect("schedule dto should parse");
        assert_eq!(dto.task_type, ScheduleTaskType::PlaylistUpdate);
    }

    #[test]
    fn defaults_to_playlist_update_when_type_is_null_or_empty() {
        let null_type: ScheduleConfigDto =
            serde_saphyr::from_str("schedule: \"0 0 * * * * *\"\ntype: ~").expect("null type should parse");
        let empty_type: ScheduleConfigDto =
            serde_saphyr::from_str("schedule: \"0 0 * * * * *\"\ntype: \"\"").expect("empty type should parse");

        assert_eq!(null_type.task_type, ScheduleTaskType::PlaylistUpdate);
        assert_eq!(empty_type.task_type, ScheduleTaskType::PlaylistUpdate);
    }

    #[test]
    fn accepts_legacy_task_type_aliases() {
        let legacy: ScheduleConfigDto = serde_saphyr::from_str("schedule: \"0 0 * * * * *\"\ntask_type: scan")
            .expect("legacy task_type alias should parse");
        assert_eq!(legacy.task_type, ScheduleTaskType::LibraryScan);
    }

    #[test]
    fn accepts_geoip_update_aliases() {
        let legacy: ScheduleConfigDto =
            serde_saphyr::from_str("schedule: \"0 0 * * * * *\"\ntype: geoip").expect("geoip alias should parse");
        assert_eq!(legacy.task_type, ScheduleTaskType::GeoIpUpdate);
    }
}
