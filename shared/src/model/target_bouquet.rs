use crate::model::{PlaylistClusterBouquetDto, XtreamCluster};

pub const TARGET_BOUQUET_VERSION: u8 = 2;

#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetBouquetMode {
    #[default]
    Whitelist,
    Blacklist,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct TargetBouquetDto {
    pub mode: TargetBouquetMode,
    pub groups: PlaylistClusterBouquetDto,
}

impl TargetBouquetDto {
    pub fn new(mode: TargetBouquetMode, mut groups: PlaylistClusterBouquetDto) -> Self {
        groups.canonicalize_for_target();
        Self { mode, groups }
    }

    pub fn whitelist(groups: PlaylistClusterBouquetDto) -> Self { Self::new(TargetBouquetMode::Whitelist, groups) }

    #[inline]
    pub fn is_unrestricted(&self) -> bool { self.groups.is_target_unrestricted() }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetBouquetFileDto {
    pub version: u8,
    pub target: String,
    #[serde(flatten)]
    pub bouquet: TargetBouquetDto,
}

impl TargetBouquetFileDto {
    pub fn new(target: impl Into<String>, bouquet: TargetBouquetDto) -> Self {
        let mut file = Self { version: TARGET_BOUQUET_VERSION, target: target.into(), bouquet };
        file.canonicalize();
        file
    }

    #[inline]
    pub fn is_unrestricted(&self) -> bool { self.bouquet.is_unrestricted() }

    pub fn canonicalize(&mut self) { self.bouquet.groups.canonicalize_for_target(); }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TargetBouquetStreamEventDto {
    Selection { bouquet: Option<TargetBouquetDto> },
    InputStarted { input: String },
    InputChunk { input: String, cluster: XtreamCluster, groups: Vec<String>, is_last_for_cluster: bool },
    InputFinished { input: String, groups: usize },
    InputWarning { input: String, message: String },
    Complete,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TargetBouquetStatusDto {
    pub name: String,
    pub mode: Option<TargetBouquetMode>,
    pub group_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_yaml_and_json() {
        let file = TargetBouquetFileDto::new(
            "family",
            TargetBouquetDto::new(
                TargetBouquetMode::Blacklist,
                PlaylistClusterBouquetDto {
                    live: Some(vec!["News".to_string(), "Kids".to_string()]),
                    vod: Some(vec!["Movies".to_string()]),
                    series: None,
                },
            ),
        );

        let json = serde_json::to_string(&file).expect("serialize json");
        let from_json: TargetBouquetFileDto = serde_json::from_str(&json).expect("deserialize json");
        assert_eq!(file, from_json);

        let yaml = serde_saphyr::to_string(&file).expect("serialize yaml");
        let from_yaml: TargetBouquetFileDto = serde_saphyr::from_str(&yaml).expect("deserialize yaml");
        assert_eq!(file, from_yaml);
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = "version: 2\ntarget: family\nmode: whitelist\nextra_field: oops\ngroups:\n  live:\n    - Kids\n";
        let result: Result<TargetBouquetFileDto, _> = serde_saphyr::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_legacy_files_without_an_explicit_mode() {
        let yaml = "version: 1\ntarget: family\ngroups:\n  live:\n    - Kids\n";
        let result: Result<TargetBouquetFileDto, _> = serde_saphyr::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn canonicalization_sorts_and_deduplicates_without_trimming_or_case_folding() {
        let mut dto = PlaylistClusterBouquetDto {
            live: Some(vec![
                " Zebra".to_string(),
                "news".to_string(),
                "News".to_string(),
                " Zebra".to_string(),
                "".to_string(),
            ]),
            vod: Some(vec![]),
            series: None,
        };

        dto.canonicalize_for_target();
        let live = dto.live.as_ref().expect("live should be Some");
        assert_eq!(live, &vec!["", " Zebra", "News", "news"]);
        assert_eq!(dto.vod, Some(Vec::new()));
        assert!(dto.series.is_none());
        assert!(!dto.is_target_unrestricted());
    }

    #[test]
    fn uncategorized_empty_string_is_restricted() {
        let mut file = TargetBouquetFileDto {
            version: TARGET_BOUQUET_VERSION,
            target: "test".to_string(),
            bouquet: TargetBouquetDto::whitelist(PlaylistClusterBouquetDto {
                live: Some(vec![String::new()]),
                vod: None,
                series: None,
            }),
        };
        file.canonicalize();
        assert!(!file.is_unrestricted());
        assert_eq!(file.bouquet.groups.live, Some(vec![String::new()]));
    }

    #[test]
    fn all_empty_is_unrestricted() {
        let mut file = TargetBouquetFileDto {
            version: TARGET_BOUQUET_VERSION,
            target: "test".to_string(),
            bouquet: TargetBouquetDto::new(
                TargetBouquetMode::Blacklist,
                PlaylistClusterBouquetDto { live: Some(vec![]), vod: Some(vec![]), series: None },
            ),
        };
        file.canonicalize();
        assert!(file.is_unrestricted());
        assert_eq!(file.bouquet.mode, TargetBouquetMode::Blacklist);
        assert_eq!(file.bouquet.groups.live, None);
        assert_eq!(file.bouquet.groups.vod, None);
        assert_eq!(file.bouquet.groups.series, None);
    }

    #[test]
    fn stream_event_serde_round_trip() {
        let event = TargetBouquetStreamEventDto::InputChunk {
            input: "provider_1".to_string(),
            cluster: XtreamCluster::Live,
            groups: vec!["News HD".to_string()],
            is_last_for_cluster: true,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let parsed: TargetBouquetStreamEventDto = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(event, parsed);
    }
}
