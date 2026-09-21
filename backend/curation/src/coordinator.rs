use crate::{
    kernel::{
        CurationEvaluation, CurationFailure, CurationProjectionCatalog, CurationRunOutcome, CurationSelectorKey,
        CurationSelectorSummary, SelectorOutcome,
    },
    tmdb::{self, TmdbClient, TmdbFailure},
    trakt,
};
use shared::model::PlaylistGroup;
use tuliprox_core::model::CurationConfig;

#[cfg(test)]
mod tests;

// Persistence compatibility at the projection edge, not the identity of a provider.
pub(crate) const LEGACY_CATEGORY_NAMESPACE: &str = "trakt-category";

fn selector_keys(config: &CurationConfig) -> (Vec<CurationSelectorKey>, Vec<CurationSelectorKey>) {
    if !config.enabled {
        return (Vec::new(), Vec::new());
    }
    let trakt_count = config
        .trakt
        .as_ref()
        .filter(|source| source.enabled)
        .map_or(0, |source| source.lists.len() + source.charts.len());
    let tmdb_count = config.tmdb.as_ref().filter(|source| source.enabled).map_or(0, |source| source.trending.len());
    (
        (0..trakt_count).map(CurationSelectorKey).collect(),
        (trakt_count..trakt_count + tmdb_count).map(CurationSelectorKey).collect(),
    )
}

/// Evaluate all required selectors against the same eligible, pre-projection catalog.
pub async fn evaluate_curation(
    http: &reqwest::Client,
    tmdb_http: Option<&reqwest::Client>,
    playlist: &[PlaylistGroup],
    target: &str,
    config: &CurationConfig,
) -> CurationRunOutcome {
    let tmdb_client =
        config.tmdb.as_ref().filter(|source| config.enabled && source.enabled && !source.trending.is_empty()).map(
            |source| tmdb_http.ok_or(TmdbFailure::Configuration).and_then(|http| TmdbClient::new(http, &source.api)),
        );
    evaluate_with_tmdb(http, playlist, target, config, tmdb_client).await
}

async fn evaluate_with_tmdb(
    http: &reqwest::Client,
    playlist: &[PlaylistGroup],
    target: &str,
    config: &CurationConfig,
    tmdb_client: Option<Result<TmdbClient, TmdbFailure>>,
) -> CurationRunOutcome {
    let (trakt_keys, tmdb_keys) = selector_keys(config);
    let mut outcomes = Vec::with_capacity(trakt_keys.len() + tmdb_keys.len());
    if !trakt_keys.is_empty() {
        if let Some(source) = &config.trakt {
            outcomes.extend(trakt::evaluate_selectors(http, playlist, target, source, &trakt_keys).await);
        }
    }
    if !tmdb_keys.is_empty() {
        if let Some(source) = &config.tmdb {
            let client = tmdb_client.expect("active TMDB selectors have a client construction outcome");
            outcomes.extend(tmdb::evaluate_selectors(client, playlist, target, source, &tmdb_keys).await);
        }
    }
    complete_evaluation(outcomes)
}

pub fn project_curation_categories(
    evaluation: &CurationEvaluation,
    playlist: &[PlaylistGroup],
    config: &CurationConfig,
) -> Vec<PlaylistGroup> {
    let (trakt_keys, tmdb_keys) = selector_keys(config);
    let catalog = CurationProjectionCatalog::new(playlist);
    let mut groups = Vec::new();
    if !trakt_keys.is_empty() {
        if let Some(source) = &config.trakt {
            groups.extend(trakt::project_categories(evaluation, &catalog, source, &trakt_keys));
        }
    }
    if !tmdb_keys.is_empty() {
        if let Some(source) = &config.tmdb {
            groups.extend(tmdb::project_categories(evaluation, &catalog, source, &tmdb_keys));
        }
    }
    groups
}

pub(crate) fn complete_evaluation(selector_outcomes: Vec<SelectorOutcome>) -> CurationRunOutcome {
    if selector_outcomes.is_empty() {
        return CurationRunOutcome::NotConfigured;
    }
    if selector_outcomes.iter().any(|outcome| !matches!(outcome, SelectorOutcome::Complete { .. })) {
        return CurationRunOutcome::Failed(CurationFailure { selector_outcomes });
    }
    let mut selectors = Vec::with_capacity(selector_outcomes.len());
    let mut memberships = Vec::new();
    for outcome in selector_outcomes {
        let SelectorOutcome::Complete { key, reference_count, memberships: mut selector_memberships } = outcome else {
            unreachable!("all selector outcomes were checked as complete")
        };
        selectors.push(CurationSelectorSummary { key, reference_count, membership_count: selector_memberships.len() });
        memberships.append(&mut selector_memberships);
    }
    CurationRunOutcome::Complete(CurationEvaluation { selectors, memberships })
}
