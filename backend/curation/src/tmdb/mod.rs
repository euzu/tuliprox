mod client;
mod model;

use crate::{
    coordinator::LEGACY_CATEGORY_NAMESPACE,
    kernel::{
        evaluate_selector, project_memberships, CurationCategorySpec, CurationEvaluation, CurationIncompleteReason,
        CurationMatchPolicy, CurationMediaScope, CurationProjectionCatalog, CurationSelectorKey, CurationSelectorSpec,
        CurationUnavailableReason, ProjectionIdentityStrategy, SelectorOutcome,
    },
};
pub(crate) use client::{TmdbClient, TmdbFailure};
use shared::model::{PlaylistGroup, TmdbTrendingKind};
use tuliprox_core::model::TmdbCurationConfig;

fn selector_spec(kind: TmdbTrendingKind) -> CurationSelectorSpec {
    CurationSelectorSpec {
        media_scope: match kind {
            TmdbTrendingKind::Movie => CurationMediaScope::Movies,
            TmdbTrendingKind::Tv => CurationMediaScope::Series,
        },
        match_policy: CurationMatchPolicy::ExactTmdbOnly,
    }
}

pub(crate) async fn evaluate_selectors(
    client: Result<TmdbClient, TmdbFailure>,
    playlist: &[PlaylistGroup],
    target: &str,
    config: &TmdbCurationConfig,
    keys: &[CurationSelectorKey],
) -> Vec<SelectorOutcome> {
    assert_eq!(keys.len(), config.trending.len());
    let mut outcomes = Vec::with_capacity(keys.len());
    for (selector, key) in config.trending.iter().zip(keys) {
        let references = match &client {
            Ok(client) => client.trending(selector).await,
            Err(error) => Err(*error),
        };
        outcomes.push(match references {
            Ok(references) => evaluate_selector(*key, &references, playlist, selector_spec(selector.kind)),
            Err(error) => {
                log::warn!("TMDB trending selector {} unavailable for target '{target}': {error:?}", key.0);
                match error {
                    TmdbFailure::Configuration => {
                        SelectorOutcome::Unavailable { key: *key, reason: CurationUnavailableReason::Configuration }
                    }
                    TmdbFailure::Body | TmdbFailure::BodyLimit => {
                        SelectorOutcome::Incomplete { key: *key, reason: CurationIncompleteReason::Interrupted }
                    }
                    _ => SelectorOutcome::Unavailable { key: *key, reason: CurationUnavailableReason::Source },
                }
            }
        });
    }
    outcomes
}

pub(crate) fn project_categories(
    evaluation: &CurationEvaluation,
    catalog: &CurationProjectionCatalog<'_>,
    config: &TmdbCurationConfig,
    keys: &[CurationSelectorKey],
) -> Vec<PlaylistGroup> {
    assert_eq!(keys.len(), config.trending.len());
    let mut groups = Vec::new();
    for (selector, key) in config.trending.iter().zip(keys) {
        if !selector.create_xtream_category {
            continue;
        }
        let Some(name) = selector.category_name.as_deref().filter(|name| !name.trim().is_empty()) else {
            continue;
        };
        let spec = CurationCategorySpec {
            name,
            selector: selector_spec(selector.kind),
            projection_identity: ProjectionIdentityStrategy::LegacyCategoryScoped {
                namespace: LEGACY_CATEGORY_NAMESPACE,
            },
        };
        groups.extend(project_memberships(
            evaluation.memberships.iter().filter(|membership| membership.selector_key == *key),
            catalog,
            &spec,
        ));
    }
    groups
}
