use crate::{
    defaults::{default_as_true, is_true},
    error::TuliproxError,
    model::{TargetOutputDto, TmdbCurationConfigDto, TraktContentType, TraktSourceConfigDto, XtreamCluster},
};
use deunicode::deunicode;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use strum_macros::{Display, EnumString};

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Display, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum CurationCatalogSelection {
    #[default]
    Full,
    Curated,
}

impl CurationCatalogSelection {
    pub const fn is_full(&self) -> bool { matches!(self, Self::Full) }

    pub const fn is_curated(self) -> bool { matches!(self, Self::Curated) }
}

/// One target-wide catalog policy with concrete discovery sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CurationConfigDto {
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "CurationCatalogSelection::is_full")]
    pub catalog_selection: CurationCatalogSelection,
    #[serde(default = "default_as_true", skip_serializing_if = "is_true")]
    pub include_xtream_base_categories: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trakt: Option<TraktSourceConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<TmdbCurationConfigDto>,
}

impl Default for CurationConfigDto {
    fn default() -> Self {
        Self {
            enabled: true,
            catalog_selection: CurationCatalogSelection::Full,
            include_xtream_base_categories: true,
            trakt: None,
            tmdb: None,
        }
    }
}

impl CurationConfigDto {
    pub fn has_active_selectors(&self) -> bool {
        self.enabled
            && (self.trakt.as_ref().is_some_and(|source| source.enabled && source.has_selectors())
                || self.tmdb.as_ref().is_some_and(|source| source.enabled && !source.trending.is_empty()))
    }

    /// Validate the policy against its outputs without changing declaration ownership.
    pub fn prepare(&mut self, outputs: &[TargetOutputDto]) -> Result<(), TuliproxError> {
        if outputs.iter().any(|output| matches!(output, TargetOutputDto::Xtream(xtream) if xtream.trakt.is_some())) {
            return Err(TuliproxError::Config(
                "target.curation and output[].trakt cannot both be configured; choose one declaration".to_string(),
            ));
        }
        if let Some(trakt) = &mut self.trakt {
            trakt.prepare(self.enabled)?;
        }
        if let Some(tmdb) = &mut self.tmdb {
            tmdb.prepare(self.enabled)?;
        }
        if !self.enabled {
            return Ok(());
        }
        if !self.has_active_selectors() && (!self.catalog_selection.is_full() || !self.include_xtream_base_categories) {
            return Err(TuliproxError::Config(
                "Enabled curation with non-default policy requires at least one active selector".to_string(),
            ));
        }

        let trakt = self.trakt.as_ref().filter(|source| source.enabled);
        let tmdb = self.tmdb.as_ref().filter(|source| source.enabled);
        let projects_categories = trakt.is_some_and(|source| trakt_projections(source).next().is_some())
            || tmdb.is_some_and(|source| source.trending.iter().any(|selector| selector.create_xtream_category));
        let has_xtream = outputs.iter().any(|output| matches!(output, TargetOutputDto::Xtream(_)));
        if !has_xtream && (projects_categories || !self.include_xtream_base_categories) {
            return Err(TuliproxError::Config(
                "Curation category projection and base-category suppression require an Xtream output".to_string(),
            ));
        }

        // Existing Trakt-only and ordinary base-group collisions retain their behavior.
        // Only new TMDB projections reserve names against all other active projections.
        let mut names = HashSet::new();
        if let Some(source) = trakt {
            for (scope, name) in trakt_projections(source) {
                let normalized = normalize_category_name(name);
                if matches!(scope, TraktContentType::Vod | TraktContentType::Both) {
                    names.insert((XtreamCluster::Video, normalized.clone()));
                }
                if matches!(scope, TraktContentType::Series | TraktContentType::Both) {
                    names.insert((XtreamCluster::Series, normalized));
                }
            }
        }
        if let Some(source) = tmdb {
            for (index, selector) in source.trending.iter().enumerate() {
                if !selector.create_xtream_category {
                    continue;
                }
                if let Some(name) = &selector.category_name {
                    let key = (selector.kind.xtream_cluster(), normalize_category_name(name));
                    if !names.insert(key) {
                        return Err(TuliproxError::Config(format!(
                            "curation.tmdb.trending[{index}].category_name conflicts with another projection in the same media cluster"
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn trakt_projections(source: &TraktSourceConfigDto) -> impl Iterator<Item = (TraktContentType, &str)> {
    source
        .lists
        .iter()
        .filter(|selector| selector.create_xtream_category)
        .filter_map(|selector| selector.category_name.as_deref().map(|name| (selector.content_type, name)))
        .chain(
            source.charts.iter().filter(|selector| selector.create_xtream_category).filter_map(|selector| {
                selector.category_name.as_deref().map(|name| (selector.kind.content_type(), name))
            }),
        )
}

fn normalize_category_name(name: &str) -> String { deunicode(name.trim()).to_lowercase() }

pub(super) fn prepare_selector_category(
    category_name: &mut Option<String>,
    create_xtream_category: bool,
    validate: bool,
    selector_name: &str,
) -> Result<(), TuliproxError> {
    *category_name = category_name.take().map(|name| name.trim().to_string()).filter(|name| !name.is_empty());
    if validate && create_xtream_category && category_name.is_none() {
        return Err(TuliproxError::Config(format!(
            "{selector_name} category_name is required when create_xtream_category is true"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        M3uTargetOutputDto, TmdbTrendingKind, TmdbTrendingTimeWindow, TraktCatalogSelection, TraktConfigDto,
        XtreamTargetOutputDto,
    };
    use serde_json::{json, Value};

    fn xtream() -> Vec<TargetOutputDto> { vec![TargetOutputDto::Xtream(XtreamTargetOutputDto::default())] }

    fn m3u() -> Vec<TargetOutputDto> { vec![TargetOutputDto::M3u(M3uTargetOutputDto::default())] }

    fn config(value: Value) -> CurationConfigDto { serde_json::from_value(value).expect("curation config shape") }

    fn tmdb_selector() -> Value { json!({"kind": "movie", "time_window": "week", "category_name": "Trending"}) }

    fn mixed() -> Value {
        json!({
            "catalog_selection": "curated",
            "trakt": {"charts": [{"kind": "movies", "chart": "popular", "category_name": "Trakt Popular"}]},
            "tmdb": {"trending": [tmdb_selector()]}
        })
    }

    #[test]
    fn curation_defaults_preserve_the_full_catalog_and_omit_default_fields() {
        let mut dto = config(json!({}));
        assert_eq!(dto, CurationConfigDto::default());
        dto.prepare(&m3u()).expect("source-less default policy is a no-op");
        assert_eq!(serde_json::to_value(dto).unwrap(), json!({}));
        assert_eq!(TraktCatalogSelection::Full, CurationCatalogSelection::Full);
        assert_eq!("CURATED".parse::<CurationCatalogSelection>().unwrap(), CurationCatalogSelection::Curated);
    }

    #[test]
    fn curation_tmdb_only_requires_no_trakt_or_metadata_credential() {
        let mut dto = config(json!({"tmdb": {"trending": [tmdb_selector()]}}));
        dto.prepare(&xtream()).expect("missing token is not a config-load failure");
        assert!(dto.trakt.is_none());
        let tmdb = dto.tmdb.unwrap();
        assert!(tmdb.api.access_token.is_empty());
        assert!(tmdb.enabled);
        assert_eq!(tmdb.trending[0].kind, TmdbTrendingKind::Movie);
        assert_eq!(tmdb.trending[0].time_window, TmdbTrendingTimeWindow::Week);
        assert_eq!(tmdb.trending[0].limit, 100);
        assert!(tmdb.trending[0].create_xtream_category);
    }

    #[test]
    fn curation_mixed_sources_round_trip_one_policy() {
        let mut dto = config(mixed());
        dto.prepare(&xtream()).unwrap();
        assert_eq!(dto.catalog_selection, CurationCatalogSelection::Curated);
        let yaml = serde_saphyr::to_string(&dto).unwrap();
        let mut restored: CurationConfigDto = serde_saphyr::from_str(&yaml).unwrap();
        restored.prepare(&xtream()).unwrap();
        assert_eq!(restored, dto);
        let value = serde_json::to_value(dto).unwrap();
        assert!(value["trakt"].get("catalog_selection").is_none());
        assert!(value["tmdb"].get("catalog_selection").is_none());
    }

    #[test]
    fn curation_selection_only_supports_m3u_and_preserves_unused_labels() {
        let mut value = mixed();
        value["trakt"]["charts"][0]["create_xtream_category"] = json!(false);
        value["tmdb"]["trending"][0]["create_xtream_category"] = json!(false);
        value["tmdb"]["trending"][0]["category_name"] = json!("  Saved label  ");
        let mut dto = config(value);
        dto.prepare(&m3u()).expect("selection does not require Xtream");
        assert_eq!(dto.tmdb.unwrap().trending[0].category_name.as_deref(), Some("Saved label"));
    }

    #[test]
    fn curation_rejects_both_owners_even_when_disabled_or_source_less() {
        for enabled in [false, true] {
            for legacy_enabled in [false, true] {
                let mut dto = config(json!({"enabled": enabled}));
                let outputs = vec![TargetOutputDto::Xtream(XtreamTargetOutputDto {
                    trakt: Some(TraktConfigDto { enabled: legacy_enabled, ..TraktConfigDto::default() }),
                    ..XtreamTargetOutputDto::default()
                })];
                let error = dto.prepare(&outputs).expect_err("two declarations must not have implicit precedence");
                let message = error.to_string();
                assert!(message.contains("curation"));
                assert!(message.contains("output[].trakt"));
            }
        }
    }

    #[test]
    fn curation_non_default_policy_needs_active_selectors() {
        for policy in [json!({"catalog_selection": "curated"}), json!({"include_xtream_base_categories": false})] {
            for sources in [json!({}), json!({"tmdb": {"enabled": false, "trending": [tmdb_selector()]}})] {
                let mut value = policy.clone();
                value.as_object_mut().unwrap().extend(sources.as_object().unwrap().clone());
                let mut dto = config(value);
                assert!(dto.prepare(&xtream()).is_err());
                dto.enabled = false;
                dto.prepare(&xtream()).expect("disabled policy is retained without execution");
            }
        }
    }

    #[test]
    fn curation_disabled_sources_do_not_invalidate_an_active_sibling() {
        let mut value = mixed();
        value["trakt"]["enabled"] = json!(false);
        value["trakt"]["charts"][0].as_object_mut().unwrap().remove("category_name");
        let mut dto = config(value);
        dto.prepare(&xtream()).expect("incomplete disabled sibling is not required");
        assert!(dto.trakt.unwrap().charts[0].category_name.is_none());
    }

    #[test]
    fn curation_disabled_block_preserves_incomplete_category_and_policy_edits() {
        let mut value = mixed();
        value["enabled"] = json!(false);
        value["include_xtream_base_categories"] = json!(false);
        value["tmdb"]["trending"][0]["category_name"] = json!("  ");
        value["trakt"]["charts"][0].as_object_mut().unwrap().remove("category_name");
        let mut dto = config(value);
        dto.prepare(&m3u()).expect("disabled projection fields may remain incomplete");
        assert_eq!(dto.catalog_selection, CurationCatalogSelection::Curated);
        assert!(!dto.include_xtream_base_categories);
        assert!(dto.tmdb.unwrap().trending[0].category_name.is_none());
    }

    #[test]
    fn curation_active_projection_requires_xtream_for_either_source() {
        for value in [
            json!({"tmdb": {"trending": [tmdb_selector()]}}),
            json!({"trakt": {"charts": [{"kind": "shows", "chart": "trending", "category_name": "Shows"}]}}),
        ] {
            let mut dto = config(value);
            assert!(dto.prepare(&m3u()).unwrap_err().to_string().contains("Xtream"));
            dto.prepare(&xtream()).expect("active projections with Xtream");
        }
    }

    #[test]
    fn curation_hiding_xtream_base_categories_requires_xtream_even_without_projections() {
        let mut selector = tmdb_selector();
        selector["create_xtream_category"] = json!(false);
        let mut dto = config(json!({"include_xtream_base_categories": false, "tmdb": {"trending": [selector]}}));
        assert!(dto.prepare(&m3u()).is_err());
        dto.prepare(&xtream()).unwrap();
    }

    #[test]
    fn curation_projection_name_is_trimmed_and_required_only_for_active_projections() {
        for name in [json!(null), json!(""), json!(" \t\n ")] {
            let mut selector = tmdb_selector();
            selector["category_name"] = name;
            let mut dto = config(json!({"tmdb": {"trending": [selector]}}));
            assert!(dto.prepare(&xtream()).unwrap_err().to_string().contains("category_name"));
            dto.tmdb.as_mut().unwrap().trending[0].create_xtream_category = false;
            dto.prepare(&m3u()).unwrap();
        }
        let mut selector = tmdb_selector();
        selector["category_name"] = json!("  Trending  ");
        let mut dto = config(json!({"tmdb": {"trending": [selector]}}));
        dto.prepare(&xtream()).unwrap();
        assert_eq!(dto.tmdb.unwrap().trending[0].category_name.as_deref(), Some("Trending"));
    }

    #[test]
    fn curation_trakt_and_tmdb_normalized_projection_collisions_are_rejected() {
        for (left, right) in [("Résumé", "  RESUME "), ("Trending", "trending")] {
            let mut value = mixed();
            value["trakt"]["charts"][0]["category_name"] = json!(left);
            value["tmdb"]["trending"][0]["category_name"] = json!(right);
            let mut dto = config(value);
            let message = dto.prepare(&xtream()).unwrap_err().to_string();
            assert!(message.contains("tmdb.trending[0].category_name"));
        }
    }

    #[test]
    fn curation_tmdb_collisions_depend_on_scope_not_current_matches() {
        let mut value = json!({"tmdb": {"trending": [tmdb_selector(), tmdb_selector()]}});
        value["tmdb"]["trending"][1]["time_window"] = json!("day");
        assert!(config(value.clone()).prepare(&xtream()).is_err());
        value["tmdb"]["trending"][1]["kind"] = json!("tv");
        config(value).prepare(&xtream()).expect("equal labels in different media clusters are allowed");
    }

    #[test]
    fn curation_trakt_both_lists_reserve_names_in_both_clusters() {
        for kind in ["movie", "tv"] {
            let mut selector = tmdb_selector();
            selector["kind"] = json!(kind);
            let mut dto = config(json!({
                "trakt": {"lists": [{"user": "alice", "list_slug": "list", "content_type": "both", "category_name": "Trending"}]},
                "tmdb": {"trending": [selector]}
            }));
            assert!(dto.prepare(&xtream()).is_err());
        }
    }

    #[test]
    fn curation_non_projecting_and_disabled_sources_reserve_no_names() {
        let mut value = mixed();
        value["trakt"]["charts"][0]["category_name"] = json!("Trending");
        for source in ["trakt", "tmdb"] {
            let mut disabled = value.clone();
            disabled[source]["enabled"] = json!(false);
            config(disabled).prepare(&xtream()).unwrap();
        }
        for (source, selectors) in [("trakt", "charts"), ("tmdb", "trending")] {
            let mut selection_only = value.clone();
            selection_only[source][selectors][0]["create_xtream_category"] = json!(false);
            config(selection_only).prepare(&xtream()).unwrap();
        }
    }

    #[test]
    fn curation_preserves_existing_trakt_only_collision_behavior() {
        let chart = json!({"kind": "movies", "chart": "trending", "category_name": "Legacy Chart"});
        let mut dto = config(json!({"trakt": {"charts": [chart.clone(), chart]}}));
        dto.prepare(&xtream()).expect("do not impose new rules on Trakt-only config");
        assert_eq!(dto.trakt.unwrap().charts.len(), 2);
    }

    #[test]
    fn curation_child_policy_and_unknown_fields_are_rejected_even_when_disabled() {
        for source in ["trakt", "tmdb"] {
            for field in ["catalog_selection", "include_xtream_base_categories", "unexpected"] {
                let value = json!({"enabled": false, source: {field: "full"}});
                assert!(serde_json::from_value::<CurationConfigDto>(value).is_err());
            }
        }
        assert!(serde_json::from_value::<CurationConfigDto>(json!({"unexpected": true})).is_err());
    }

    #[test]
    fn curation_tmdb_requires_explicit_kind_and_window_even_when_disabled() {
        for field in ["kind", "time_window"] {
            let mut selector = tmdb_selector();
            selector.as_object_mut().unwrap().remove(field);
            let value = json!({"enabled": false, "tmdb": {"trending": [selector]}});
            assert!(serde_json::from_value::<CurationConfigDto>(value).is_err(), "missing {field}");
        }
        for (field, invalid) in [("kind", "all"), ("time_window", "month")] {
            let mut selector = tmdb_selector();
            selector[field] = json!(invalid);
            let value = json!({"enabled": false, "tmdb": {"trending": [selector]}});
            assert!(serde_json::from_value::<CurationConfigDto>(value).is_err(), "unsupported {field}");
        }
    }

    #[test]
    fn curation_tmdb_accepts_all_four_bounded_feed_shapes() {
        for kind in ["movie", "tv"] {
            for window in ["day", "week"] {
                let value = json!({"tmdb": {"trending": [{"kind": kind, "time_window": window, "create_xtream_category": false}]}});
                let mut dto = config(value);
                dto.prepare(&m3u()).unwrap();
            }
        }
    }

    #[test]
    fn curation_tmdb_debug_redacts_tokens_without_losing_serialized_credentials() {
        let secret = "private-test-token";
        let mut dto =
            config(json!({"tmdb": {"api": {"access_token": format!("  {secret}  ")}, "trending": [tmdb_selector()]}}));
        dto.prepare(&xtream()).unwrap();
        assert!(!format!("{dto:?}").contains(secret));
        assert_eq!(dto.tmdb.as_ref().unwrap().api.access_token, secret);
        assert_eq!(serde_json::to_value(&dto).unwrap()["tmdb"]["api"]["access_token"], secret);
        dto.tmdb.as_mut().unwrap().trending[0].category_name = None;
        assert!(!dto.prepare(&xtream()).unwrap_err().to_string().contains(secret));
    }

    #[test]
    fn curation_tmdb_header_validity_belongs_to_runtime_not_source_loading() {
        for token in ["", "  ", "invalid\nheader"] {
            let mut dto = config(json!({"tmdb": {"api": {"access_token": token}, "trending": [tmdb_selector()]}}));
            dto.prepare(&xtream()).expect("credential availability must not invalidate the containing source config");
        }
    }

    #[test]
    fn curation_tmdb_does_not_accept_metadata_or_arbitrary_endpoint_options() {
        for field in ["api_key", "url", "language", "page", "limit"] {
            let value = json!({"tmdb": {"api": {field: "not-a-supported-option"}}});
            assert!(serde_json::from_value::<CurationConfigDto>(value).is_err(), "unsupported API field {field}");
        }
    }
}
