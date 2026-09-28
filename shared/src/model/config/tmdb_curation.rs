use super::curation::prepare_selector_category;
use crate::{
    defaults::{default_as_true, is_true},
    error::TuliproxError,
    model::XtreamCluster,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// TMDB application Read Access Token. Serialization is lossless; diagnostics are not.
#[derive(Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TmdbCurationApiConfigDto {
    #[serde(default)]
    pub access_token: String,
}

impl fmt::Debug for TmdbCurationApiConfigDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TmdbCurationApiConfigDto").field("access_token", &"[redacted]").finish()
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TmdbTrendingKind {
    Movie,
    Tv,
}

impl TmdbTrendingKind {
    pub const fn xtream_cluster(self) -> XtreamCluster {
        match self {
            Self::Movie => XtreamCluster::Video,
            Self::Tv => XtreamCluster::Series,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TmdbTrendingTimeWindow {
    Day,
    Week,
}

pub const TMDB_TRENDING_DEFAULT_LIMIT: u32 = 100;
pub const TMDB_TRENDING_MAX_LIMIT: u32 = 500;

const fn default_trending_limit() -> u32 { TMDB_TRENDING_DEFAULT_LIMIT }
const fn is_default_trending_limit(limit: &u32) -> bool { *limit == TMDB_TRENDING_DEFAULT_LIMIT }
pub const fn is_valid_tmdb_trending_limit(limit: u32) -> bool { limit >= 1 && limit <= TMDB_TRENDING_MAX_LIMIT }

// deserialize_u32 permits quoted numbers in our YAML loader. Inspect the scalar type
// instead so YAML and JSON have the same integer-only contract (including disabled drafts).
fn deserialize_trending_limit<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    struct Integer;
    impl serde::de::Visitor<'_> for Integer {
        type Value = u32;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("an unsigned integer") }
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<u32, E> {
            u32::try_from(value).map_err(|_| E::custom("TMDB trending limit exceeds u32"))
        }
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<u32, E> {
            u32::try_from(value).map_err(|_| E::custom("TMDB trending limit must be unsigned and fit u32"))
        }
    }
    deserializer.deserialize_any(Integer)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TmdbTrendingConfigDto {
    pub kind: TmdbTrendingKind,
    pub time_window: TmdbTrendingTimeWindow,
    /// Unique remote references before local matching, not a desired number of matches.
    #[serde(
        default = "default_trending_limit",
        deserialize_with = "deserialize_trending_limit",
        skip_serializing_if = "is_default_trending_limit"
    )]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_name: Option<String>,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub create_xtream_category: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TmdbCurationConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default)]
    pub api: TmdbCurationApiConfigDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trending: Vec<TmdbTrendingConfigDto>,
}

impl Default for TmdbCurationConfigDto {
    fn default() -> Self { Self { enabled: true, api: TmdbCurationApiConfigDto::default(), trending: Vec::new() } }
}

#[cfg(test)]
mod tests {
    use crate::model::ConfigTargetDto;

    fn target(selector_fields: &str, target_enabled: bool, curation_enabled: bool, source_enabled: bool) -> String {
        format!(
            r#"{{"name":"discovery","enabled":{target_enabled},"output":[{{"type":"xtream"}}],
            "curation":{{"enabled":{curation_enabled},"tmdb":{{"enabled":{source_enabled},"trending":[
            {{"kind":"movie","time_window":"week","category_name":"Movies"{selector_fields}}}]}}}}}}"#
        )
    }

    #[test]
    fn curation_tmdb_item_limit_defaults_and_boundaries_round_trip() {
        for (fields, limit) in [("", 100), (",\"limit\":100", 100), (",\"limit\":1", 1), (",\"limit\":500", 500)] {
            for yaml in [false, true] {
                let document = target(fields, true, true, true);
                let mut dto: ConfigTargetDto = if yaml {
                    serde_saphyr::from_str(&document).unwrap()
                } else {
                    serde_json::from_str(&document).unwrap()
                };
                dto.prepare(1, None, None).unwrap();
                let value = serde_json::to_value(&dto).unwrap();
                let selector = &value["curation"]["tmdb"]["trending"][0];
                if limit == 100 {
                    assert!(selector.get("limit").is_none());
                } else {
                    assert_eq!(selector["limit"], limit);
                }
                let restored: ConfigTargetDto =
                    serde_saphyr::from_str(&serde_saphyr::to_string(&dto).unwrap()).unwrap();
                assert_eq!(restored.curation, dto.curation);
            }
        }
    }

    #[test]
    fn curation_tmdb_item_limit_rejects_types_and_ranges_even_when_disabled() {
        for raw in [
            "null",
            "0",
            "-1",
            "501",
            "4294967295",
            "4294967296",
            "18446744073709551616",
            "1.5",
            "100.0",
            "true",
            "\"100\"",
            "[]",
            "{}",
        ] {
            for switches in [(true, true, true), (false, true, true), (true, false, true), (true, true, false)] {
                let document = target(&format!(",\"limit\":{raw}"), switches.0, switches.1, switches.2);
                for mut parsed in [
                    serde_json::from_str::<ConfigTargetDto>(&document).ok(),
                    serde_saphyr::from_str::<ConfigTargetDto>(&document).ok(),
                ] {
                    if let Some(dto) = &mut parsed {
                        assert!(dto.prepare(1, None, None).is_err(), "accepted {raw}, switches {switches:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn curation_tmdb_item_limit_rejects_scope_and_paging_fields_even_when_disabled() {
        for fields in [
            ",\"scope\":\"first_page\"",
            ",\"scope\":null",
            ",\"scope\":\"first_page\",\"limit\":100",
            ",\"page\":1",
            ",\"max_pages\":5",
        ] {
            for enabled in [false, true] {
                let document = target(fields, enabled, enabled, enabled);
                assert!(serde_json::from_str::<ConfigTargetDto>(&document).is_err(), "{fields}");
                assert!(serde_saphyr::from_str::<ConfigTargetDto>(&document).is_err(), "{fields}");
            }
        }
    }
}

impl TmdbCurationConfigDto {
    pub(super) fn prepare(&mut self, curation_enabled: bool) -> Result<(), TuliproxError> {
        self.api.access_token = self.api.access_token.trim().to_string();
        for selector in &mut self.trending {
            // Even disabled editor drafts must retain a valid acquisition bound.
            if !is_valid_tmdb_trending_limit(selector.limit) {
                return Err(TuliproxError::Config("TMDB trending limit must be an integer in 1..=500".to_string()));
            }
            prepare_selector_category(
                &mut selector.category_name,
                selector.create_xtream_category,
                curation_enabled && self.enabled,
                "TMDB trending",
            )?;
        }
        Ok(())
    }
}
