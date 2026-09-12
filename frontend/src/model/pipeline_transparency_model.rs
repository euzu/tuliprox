use crate::model::{InputUpdateCardModel, InputUpdateCardStatus, PlaylistUpdateCardStatuses};
use shared::model::{
    ConfigInputDto, ConfigTargetDto, InputRefreshPolicy, InputType, LibraryScanResult, LibraryStatus,
    PersistedPlaylistUpdateClusterSnapshot, PersistedPlaylistUpdateClusterState,
    PersistedPlaylistUpdateClusterStatusDto, PersistedPlaylistUpdateQualityDecision,
    PersistedPlaylistUpdateTechnicalState, PlaylistUpdateClusterDecision, PlaylistUpdateDataSource,
    PlaylistUpdateInputTelemetry, PlaylistUpdateRunId, PlaylistUpdateRunOrder, PlaylistUpdateStatusDto,
    ProcessingOrder, SourcesConfigDto, XtreamCluster,
};
use std::{collections::HashMap, sync::Arc};

/// Frontend read model assembled from the existing configuration and update-run lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputUpdateRunView {
    /// Current canonical catalog, independent of the correlated run's scan metrics.
    pub library_catalog: Option<LibraryStatus>,
    /// Real result for this effective run/input only; never reconstructed after reload.
    pub library_scan_result: Option<LibraryScanResult>,
    pub input_id: u16,
    pub input_name: Arc<str>,
    pub run_id: Option<PlaylistUpdateRunId>,
    pub execution_order: Option<PlaylistUpdateRunOrder>,
    pub overall_state: InputUpdateCardStatus,
    /// Input acquisition outcome; an input success does not prove target publication.
    pub input_state: InputUpdateCardStatus,
    pub refresh_policy: Option<InputRefreshPolicy>,
    pub source_type: InputType,
    /// Actual source of a non-cluster input, when existing runtime data proves it.
    pub cache_source: Option<InputDataSourceView>,
    pub last_update_at: Option<u64>,
    pub progress_details: Vec<String>,
    pub cluster_results: Vec<InputClusterRunView>,
    pub target_results: Vec<TargetRunView>,
}

impl InputUpdateRunView {
    /// Content proven by the canonical catalog, not a fabricated per-cluster run outcome.
    #[must_use]
    pub fn catalog_content_clusters(&self) -> Vec<XtreamCluster> {
        self.library_catalog.as_ref().map_or_else(Vec::new, |catalog| {
            [(XtreamCluster::Series, catalog.series), (XtreamCluster::Video, catalog.movies)]
                .into_iter()
                .filter_map(|(cluster, count)| (count > 0).then_some(cluster))
                .collect()
        })
    }
    /// Attaches the existing Library status read without changing run correlation.
    #[must_use]
    pub fn with_library_catalog(mut self, catalog: Option<&LibraryStatus>) -> Self {
        self.library_catalog =
            catalog.filter(|catalog| self.source_type == InputType::Library && catalog.enabled).cloned();
        self
    }
}

/// Available configuration and optional population facts for one real input cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputClusterRunView {
    pub cluster: XtreamCluster,
    pub enabled_by_configuration: bool,
    /// Actual run selection when reported by the processing lifecycle.
    pub requested: Option<bool>,
    /// Current run policy or the request-local policy retained by this cluster's last snapshot.
    pub refresh_policy: Option<InputRefreshPolicy>,
    /// `None` means that the configured quality guard is disabled for this cluster.
    pub threshold: Option<u8>,
    pub baseline_count: Option<usize>,
    pub candidate_count: Option<usize>,
    pub active_count: Option<usize>,
    pub quality: Option<u8>,
    /// Actual source used for this cluster; never inferred from the selected policy.
    pub source: Option<InputDataSourceView>,
    /// Typed runtime outcome; never recalculated from `quality` and `threshold`.
    pub outcome: Option<InputClusterOutcomeView>,
    /// Persisted technical completion, kept separate from the Quality outcome.
    pub technical_state: Option<PersistedPlaylistUpdateTechnicalState>,
    /// Last canonical persisted result, present only when no correlated run determines this card.
    pub persisted_status: Option<PersistedClusterStatusView>,
}

/// Minimal reload fallback projected from the canonical persisted input status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistedClusterStatusView {
    pub status: PersistedPlaylistUpdateClusterState,
    pub timestamp: u64,
    pub last_update: Option<PersistedPlaylistUpdateClusterSnapshot>,
}

/// Proven origin of the input population used by a run.
pub type InputDataSourceView = PlaylistUpdateDataSource;

/// Proven cluster result supplied by the run lifecycle.
pub type InputClusterOutcomeView = PlaylistUpdateClusterDecision;

/// Target configuration related to one input strictly through stable IDs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetRunView {
    pub target_id: u16,
    pub target_name: String,
    pub input_ids: Vec<u16>,
    pub pipeline: Option<TargetPipelineView>,
}

/// Existing target processing configuration; no execution telemetry is inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetPipelineView {
    pub processing_order: ProcessingOrder,
    pub filter_summary: RuleStageSummary,
    pub rename_summary: RuleStageSummary,
    pub mapping_summary: RuleStageSummary,
}

/// Count of configured entries for one target stage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuleStageSummary {
    pub configured_count: usize,
}

/// Aggregates existing frontend runtime and configuration data without retaining another run history.
#[must_use]
pub fn build_input_update_run_views(
    sources: &SourcesConfigDto,
    cards: &[InputUpdateCardModel],
    statuses: &PlaylistUpdateCardStatuses,
    persisted_statuses: &PlaylistUpdateStatusDto,
) -> Vec<InputUpdateRunView> {
    let input_config_by_id = sources.inputs.iter().map(|input| (input.id, input)).collect::<HashMap<_, _>>();
    let target_config_by_id = sources
        .sources
        .iter()
        .flat_map(|source| &source.targets)
        .map(|target| (target.id, target))
        .collect::<HashMap<_, _>>();
    let input_ids_by_target_id = input_ids_by_target_id(cards);
    let persisted_by_input_id =
        persisted_statuses.inputs.iter().map(|status| (status.input_id, status)).collect::<HashMap<_, _>>();

    cards
        .iter()
        .map(|card| {
            let runtime = statuses.for_input(card.input_id);
            let persisted = persisted_by_input_id.get(&card.input_id).filter(|_| runtime.run_id().is_none());
            let persisted_clusters = persisted.map_or(&[][..], |status| status.clusters.as_slice());
            let input_state = persisted
                .and_then(|status| status.last_input_update)
                .map_or(runtime.status(), |result| super::playlist_update_card_status::status_from_state(result.state));
            let cluster_results = input_config_by_id.get(&card.input_id).map_or_else(Vec::new, |input| {
                cluster_views(input, card.input_type, runtime.input_telemetry(), persisted_clusters)
            });
            let target_results = card
                .targets
                .iter()
                .map(|target| TargetRunView {
                    target_id: target.id,
                    target_name: target.name.clone(),
                    input_ids: input_ids_by_target_id.get(&target.id).cloned().unwrap_or_default(),
                    pipeline: target_config_by_id.get(&target.id).map(|config| target_pipeline(config)),
                })
                .collect();

            InputUpdateRunView {
                library_catalog: None,
                library_scan_result: (card.input_type == InputType::Library)
                    .then(|| runtime.library_scan_result().cloned())
                    .flatten(),
                input_id: card.input_id,
                input_name: Arc::clone(&card.input_name),
                run_id: runtime.run_id().cloned(),
                execution_order: runtime.execution_order(),
                overall_state: runtime.action_status(),
                input_state,
                refresh_policy: runtime.refresh_policy(),
                source_type: card.input_type,
                cache_source: runtime.input_telemetry().and_then(|telemetry| telemetry.source),
                last_update_at: card.last_update_at,
                progress_details: runtime.details().to_vec(),
                cluster_results,
                target_results,
            }
        })
        .collect()
}

fn input_ids_by_target_id(cards: &[InputUpdateCardModel]) -> HashMap<u16, Vec<u16>> {
    let mut input_ids = HashMap::<u16, Vec<u16>>::new();
    for card in cards {
        for target in &card.targets {
            let target_input_ids = input_ids.entry(target.id).or_default();
            if !target_input_ids.contains(&card.input_id) {
                target_input_ids.push(card.input_id);
            }
        }
    }
    input_ids
}

fn cluster_views(
    input: &ConfigInputDto,
    input_type: InputType,
    input_telemetry: Option<&PlaylistUpdateInputTelemetry>,
    persisted_statuses: &[PersistedPlaylistUpdateClusterStatusDto],
) -> Vec<InputClusterRunView> {
    let observed_clusters_only = input_type.is_m3u() || input_type == InputType::Staged;
    if !(input_type.is_xtream() || input_type.is_stalker() || observed_clusters_only) {
        return Vec::new();
    }

    let options = input.options.as_ref();
    let configured = [
        (
            XtreamCluster::Live,
            options.is_none_or(|value| !value.skip_live),
            options.map_or(0, |value| value.update_quality.live),
        ),
        (
            XtreamCluster::Video,
            options.is_none_or(|value| !value.skip_vod),
            options.map_or(0, |value| value.update_quality.vod),
        ),
        (
            XtreamCluster::Series,
            options.is_none_or(|value| !value.skip_series),
            options.map_or(0, |value| value.update_quality.series),
        ),
    ];
    configured
        .into_iter()
        .filter(|(cluster, _, _)| {
            !observed_clusters_only
                || input_telemetry
                    .is_some_and(|telemetry| telemetry.clusters.iter().any(|fact| fact.cluster == *cluster))
                || persisted_statuses.iter().any(|fact| fact.cluster == *cluster)
        })
        .map(|(cluster, enabled_by_configuration, threshold)| {
            let runtime = input_telemetry.and_then(|telemetry| {
                telemetry.clusters.iter().find(|runtime_cluster| runtime_cluster.cluster == cluster)
            });
            let persisted_status = if runtime.is_none() {
                persisted_statuses.iter().find(|persisted| persisted.cluster == cluster).map(|persisted| {
                    PersistedClusterStatusView {
                        status: persisted.status,
                        timestamp: persisted.timestamp,
                        last_update: persisted.last_update,
                    }
                })
            } else {
                None
            };
            let snapshot = persisted_status.and_then(|persisted| persisted.last_update);
            let configured_threshold = (threshold > 0).then_some(threshold);
            let (
                refresh_policy,
                threshold,
                baseline_count,
                candidate_count,
                active_count,
                quality,
                source,
                outcome,
                technical_state,
            ) = if let Some(runtime) = runtime {
                (
                    input_telemetry.map(|telemetry| telemetry.refresh_policy),
                    runtime.threshold,
                    runtime.baseline_count,
                    runtime.candidate_count,
                    runtime.active_count,
                    runtime.quality,
                    runtime.source,
                    runtime.decision,
                    runtime.technical_state,
                )
            } else if let Some(snapshot) = snapshot {
                let quality = snapshot.quality;
                let historical_threshold = quality.map(|quality| quality.threshold).or_else(|| {
                    snapshot
                        .quality_guard_threshold
                        .map_or(configured_threshold, |threshold| (threshold > 0).then_some(threshold))
                });
                (
                    snapshot.policy,
                    historical_threshold,
                    quality.and_then(|quality| quality.baseline_count),
                    quality.and_then(|quality| quality.candidate_count),
                    snapshot.active_count,
                    quality.and_then(|quality| quality.achieved_quality),
                    snapshot.source,
                    quality.map(|quality| match quality.decision {
                        PersistedPlaylistUpdateQualityDecision::Accepted => PlaylistUpdateClusterDecision::Accepted,
                        PersistedPlaylistUpdateQualityDecision::Rejected => PlaylistUpdateClusterDecision::Rejected,
                    }),
                    snapshot.technical_state,
                )
            } else {
                (None, configured_threshold, None, None, None, None, None, None, None)
            };
            InputClusterRunView {
                cluster,
                enabled_by_configuration,
                requested: runtime.map(|runtime| runtime.requested),
                refresh_policy,
                threshold,
                baseline_count,
                candidate_count,
                active_count,
                quality,
                source,
                outcome,
                technical_state,
                persisted_status,
            }
        })
        .collect()
}

fn target_pipeline(target: &ConfigTargetDto) -> TargetPipelineView {
    TargetPipelineView {
        processing_order: target.processing_order,
        filter_summary: RuleStageSummary { configured_count: configured_filter_count(target) },
        rename_summary: RuleStageSummary { configured_count: target.rename.as_ref().map_or(0, Vec::len) },
        mapping_summary: RuleStageSummary { configured_count: target.mapping.as_ref().map_or(0, Vec::len) },
    }
}

fn configured_filter_count(target: &ConfigTargetDto) -> usize {
    [target.filter.processing.as_deref(), target.filter.persist.as_deref()]
        .into_iter()
        .flatten()
        .filter(|filter| !filter.trim().is_empty())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{
            InputUpdateCapabilities, InputUpdateCapabilitiesExt, InputUpdateCardTarget, PlaylistUpdateAcceptedScope,
            PlaylistUpdateCardStatusAction,
        },
        services::WebSocketConnectionContext,
    };
    use shared::{
        model::{
            ConfigInputOptionsDto, ConfigInputUpdateQualityDto, ConfigRenameDto, ConfigSourceDto,
            ConfigTargetFilterDto, InputPlaylistUpdateStatusDto, ItemField, PersistedPlaylistUpdateQualitySnapshot,
            PlaylistUpdateClusterTelemetry, PlaylistUpdateProgressEvent, PlaylistUpdateRunStateEvent,
            PlaylistUpdateState,
        },
        utils::Internable,
    };
    use std::rc::Rc;
    use yew::Reducible;

    fn input(id: u16, name: &str, input_type: InputType) -> ConfigInputDto {
        ConfigInputDto { id, name: name.intern(), input_type, enabled: true, ..ConfigInputDto::default() }
    }

    #[test]
    fn update_overview_badges_m3u_reload_uses_typed_quality_and_live_run_wins() {
        let input = input(27, "m3u", InputType::M3u);
        let cards = vec![card(&input, &[], Some(123))];
        let sources = SourcesConfigDto { inputs: vec![input], ..SourcesConfigDto::default() };
        let mut persisted =
            persisted_status(27, &[(XtreamCluster::Series, PersistedPlaylistUpdateClusterState::Failed, 123)]);
        persisted.inputs[0].clusters[0].last_update = Some(PersistedPlaylistUpdateClusterSnapshot {
            policy: Some(InputRefreshPolicy::REFRESH),
            source: Some(InputDataSourceView::Provider),
            quality: Some(PersistedPlaylistUpdateQualitySnapshot {
                threshold: 90,
                baseline_count: Some(10),
                candidate_count: Some(0),
                achieved_quality: Some(0),
                decision: PersistedPlaylistUpdateQualityDecision::Rejected,
            }),
            active_count: Some(10),
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
            ..PersistedPlaylistUpdateClusterSnapshot::default()
        });
        let project = |state: &PlaylistUpdateCardStatuses| {
            build_input_update_run_views(&sources, &cards, state, &persisted).remove(0)
        };
        let reload = project(&PlaylistUpdateCardStatuses::default());
        assert_eq!(reload.cluster_results.len(), 1, "Do not invent other content from the input type");
        let series = reload.cluster_results[0];
        assert_eq!(series.outcome, Some(InputClusterOutcomeView::Rejected));
        assert_eq!((series.threshold, series.quality, series.active_count), (Some(90), Some(0), Some(10)));
        assert_eq!(series.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));
        let connection = WebSocketConnectionContext::new(1, true);
        let queued = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection)).reduce(
            PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: connection,
                run_id: "new-m3u".into(),
                input_ids: vec![27],
                refresh_policy: InputRefreshPolicy::NORMAL,
            }),
        );
        assert_eq!(project(&queued).overall_state, InputUpdateCardStatus::Queued);
        assert!(project(&queued).cluster_results.is_empty(), "A queued run inherits no prior cluster outcome");
        let active = queued.reduce(PlaylistUpdateCardStatusAction::Progress {
            connection_context: connection,
            progress: PlaylistUpdateProgressEvent::for_run_input("new-m3u".into(), 2.into(), 27, "m3u", "current")
                .with_input_telemetry(PlaylistUpdateInputTelemetry {
                    refresh_policy: InputRefreshPolicy::NORMAL,
                    source: None,
                    clusters: vec![PlaylistUpdateClusterTelemetry {
                        cluster: XtreamCluster::Series,
                        requested: true,
                        source: Some(InputDataSourceView::Cache),
                        baseline_count: None,
                        candidate_count: None,
                        active_count: None,
                        threshold: None,
                        quality: None,
                        decision: None,
                        technical_state: None,
                    }],
                }),
        });
        let current = project(&active);
        assert_eq!(current.run_id.as_ref().map(AsRef::as_ref), Some("new-m3u"));
        assert_eq!(current.overall_state, InputUpdateCardStatus::Updating);
        assert_eq!(current.cluster_results[0].source, Some(InputDataSourceView::Cache));
        assert_eq!(current.cluster_results[0].persisted_status, None);
        assert_eq!(current.cluster_results[0].outcome, None);
    }

    #[test]
    fn pipeline_transparency_playlist_update_status_delayed_sources_agree_before_and_after_reload() {
        use shared::model::PersistedPlaylistUpdateInputResult;
        for weaker in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial] {
            for failure_first in [false, true] {
                let input = input(7, "renamed-shared-input", InputType::M3u);
                let cards = vec![card(&input, &[], Some(123))];
                let sources = SourcesConfigDto { inputs: vec![input], ..SourcesConfigDto::default() };
                let connection = WebSocketConnectionContext::new(1, true);
                let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection));
                let results = if failure_first {
                    [PlaylistUpdateState::Failure, weaker]
                } else {
                    [weaker, PlaylistUpdateState::Failure]
                };
                for result in results {
                    state = state.reduce(PlaylistUpdateCardStatusAction::Progress {
                        connection_context: connection,
                        progress: PlaylistUpdateProgressEvent::input_completed(
                            "shared-run".into(),
                            1.into(),
                            7,
                            result,
                            "old-name",
                            "source completion",
                        ),
                    });
                }
                let live = build_input_update_run_views(&sources, &cards, &state, &PlaylistUpdateStatusDto::default())
                    .remove(0);
                assert_eq!(live.input_state, InputUpdateCardStatus::Failed);
                assert_eq!(live.run_id, Some("shared-run".into()));
                // The canonical writer's order-independent result, carried through
                // the existing DTO; a reload has no run or target facts to infer.
                let persisted = PlaylistUpdateStatusDto {
                    inputs: vec![InputPlaylistUpdateStatusDto {
                        input_id: 7,
                        last_update: Some(123),
                        clusters: Vec::new(),
                        last_input_update: Some(PersistedPlaylistUpdateInputResult {
                            state: PlaylistUpdateState::Failure,
                            timestamp: 456,
                        }),
                    }],
                    ..PlaylistUpdateStatusDto::default()
                };
                let wire = serde_json::to_vec(&persisted).unwrap();
                let reloaded = build_input_update_run_views(
                    &sources,
                    &cards,
                    &PlaylistUpdateCardStatuses::default(),
                    &serde_json::from_slice(&wire).unwrap(),
                )
                .remove(0);
                assert_eq!(reloaded.input_state, live.input_state);
                assert_eq!(reloaded.overall_state, InputUpdateCardStatus::Ready);
                assert_eq!(reloaded.run_id, None);
                assert_eq!(reloaded.cache_source, None);
                assert!(reloaded.cluster_results.is_empty());
            }
        }
    }

    #[test]
    fn pipeline_transparency_playlist_update_status_staged_reload_uses_own_id_not_parent_result() {
        use shared::model::PersistedPlaylistUpdateInputResult;
        let parent = input(7, "parent", InputType::Xtream);
        let staged = input(8, "renamed-staged", InputType::Staged);
        let cards = vec![card(&staged, &[], Some(123)), card(&parent, &[], Some(123))];
        let sources = SourcesConfigDto { inputs: vec![parent, staged], ..SourcesConfigDto::default() };
        for (own, parent_state, expected) in [
            (Some(PlaylistUpdateState::Success), PlaylistUpdateState::Failure, InputUpdateCardStatus::Success),
            (Some(PlaylistUpdateState::Failure), PlaylistUpdateState::Success, InputUpdateCardStatus::Failed),
            (None, PlaylistUpdateState::Success, InputUpdateCardStatus::Ready),
        ] {
            let persisted = PlaylistUpdateStatusDto {
                inputs: vec![
                    InputPlaylistUpdateStatusDto {
                        input_id: 7,
                        last_update: Some(123),
                        clusters: Vec::new(),
                        last_input_update: Some(PersistedPlaylistUpdateInputResult {
                            state: parent_state,
                            timestamp: 456,
                        }),
                    },
                    InputPlaylistUpdateStatusDto {
                        input_id: 8,
                        last_update: Some(123),
                        clusters: vec![PersistedPlaylistUpdateClusterStatusDto {
                            cluster: XtreamCluster::Live,
                            status: PersistedPlaylistUpdateClusterState::Ok,
                            timestamp: 123,
                            last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                                policy: Some(InputRefreshPolicy::REFRESH),
                                source: Some(PlaylistUpdateDataSource::Cache),
                                quality_guard_threshold: Some(95),
                                ..PersistedPlaylistUpdateClusterSnapshot::default()
                            }),
                        }],
                        last_input_update: own
                            .map(|state| PersistedPlaylistUpdateInputResult { state, timestamp: 456 }),
                    },
                ],
                ..PlaylistUpdateStatusDto::default()
            };
            let view =
                build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted)
                    .remove(0);
            assert_eq!(view.input_id, 8);
            assert_eq!(view.input_state, expected);
            assert_eq!(view.cluster_results.len(), 1);
            assert_eq!(view.cluster_results[0].cluster, XtreamCluster::Live);
            assert_eq!(view.cluster_results[0].source, Some(PlaylistUpdateDataSource::Cache));
            assert_eq!(view.cache_source, None);
            assert_eq!(view.refresh_policy, None);
            assert_eq!(view.overall_state, InputUpdateCardStatus::Ready);
        }
    }

    #[test]
    fn pipeline_transparency_persisted_input_status_restores_all_types_without_runtime_or_target_inference() {
        use shared::model::PersistedPlaylistUpdateInputResult;
        for input_type in [
            InputType::Xtream,
            InputType::XtreamBatch,
            InputType::Stalker,
            InputType::StalkerBatch,
            InputType::M3u,
            InputType::M3uBatch,
            InputType::Library,
            InputType::Plex,
            InputType::Emby,
            InputType::Jellyfin,
            InputType::Staged,
        ] {
            for enabled in [false, true] {
                let mut input = input(7, "renamed-display-name", input_type);
                input.enabled = enabled;
                let cards = vec![card(&input, &[], Some(123))];
                let sources = SourcesConfigDto { inputs: vec![input], ..SourcesConfigDto::default() };
                for (state, expected) in [
                    (PlaylistUpdateState::Success, InputUpdateCardStatus::Success),
                    (PlaylistUpdateState::Partial, InputUpdateCardStatus::Partial),
                    (PlaylistUpdateState::Failure, InputUpdateCardStatus::Failed),
                ] {
                    let persisted = PlaylistUpdateStatusDto {
                        inputs: vec![InputPlaylistUpdateStatusDto {
                            input_id: 7,
                            last_update: Some(123),
                            last_input_update: Some(PersistedPlaylistUpdateInputResult { state, timestamp: 456 }),
                            clusters: Vec::new(),
                        }],
                        ..PlaylistUpdateStatusDto::default()
                    };
                    let view = build_input_update_run_views(
                        &sources,
                        &cards,
                        &PlaylistUpdateCardStatuses::default(),
                        &persisted,
                    )
                    .remove(0);
                    assert_eq!(view.input_state, expected, "{input_type}");
                    assert_eq!(
                        view.overall_state,
                        InputUpdateCardStatus::Ready,
                        "input completion does not prove target publication"
                    );
                    assert_eq!(view.last_update_at, Some(123));
                    assert_eq!(view.run_id, None);
                    assert_eq!(view.cache_source, None);
                    assert_eq!(view.refresh_policy, None);
                    assert_eq!(view.library_scan_result, None);
                    assert!(view.progress_details.is_empty());
                    assert_eq!(view.cluster_results.is_empty(), !(input_type.is_xtream() || input_type.is_stalker()));
                    assert!(view.cluster_results.iter().all(|cluster| cluster.outcome.is_none()
                        && cluster.quality.is_none()
                        && cluster.source.is_none()));
                    let unrelated = PlaylistUpdateStatusDto {
                        inputs: vec![InputPlaylistUpdateStatusDto { input_id: 99, ..persisted.inputs[0].clone() }],
                        ..PlaylistUpdateStatusDto::default()
                    };
                    assert_eq!(
                        build_input_update_run_views(
                            &sources,
                            &cards,
                            &PlaylistUpdateCardStatuses::default(),
                            &unrelated
                        )[0]
                        .input_state,
                        InputUpdateCardStatus::Ready
                    );
                }
                assert_eq!(
                    build_input_update_run_views(
                        &sources,
                        &cards,
                        &PlaylistUpdateCardStatuses::default(),
                        &PlaylistUpdateStatusDto::default()
                    )[0]
                    .input_state,
                    InputUpdateCardStatus::Ready
                );
            }
        }
    }

    #[test]
    fn pipeline_transparency_persisted_input_status_never_supplements_current_run_and_reloads_after_reset() {
        use shared::model::PersistedPlaylistUpdateInputResult;
        for input_type in [InputType::M3u, InputType::Library, InputType::Plex, InputType::Xtream, InputType::Stalker] {
            let input = input(7, "input", input_type);
            let cards = vec![card(&input, &[], Some(123))];
            let sources = SourcesConfigDto { inputs: vec![input], ..SourcesConfigDto::default() };
            let mut persisted = PlaylistUpdateStatusDto {
                inputs: vec![InputPlaylistUpdateStatusDto {
                    input_id: 7,
                    last_update: Some(123),
                    clusters: Vec::new(),
                    last_input_update: Some(PersistedPlaylistUpdateInputResult {
                        state: PlaylistUpdateState::Failure,
                        timestamp: 123,
                    }),
                }],
                ..PlaylistUpdateStatusDto::default()
            };
            let connection = WebSocketConnectionContext::new(1, true);
            let mut state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection)).reduce(
                PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                    connection_context: connection,
                    run_id: "current".into(),
                    input_ids: vec![7],
                    refresh_policy: InputRefreshPolicy::REFRESH,
                }),
            );
            for expected in
                [InputUpdateCardStatus::Queued, InputUpdateCardStatus::Updating, InputUpdateCardStatus::Success]
            {
                let view = build_input_update_run_views(&sources, &cards, &state, &persisted).remove(0);
                assert_eq!(view.input_state, expected);
                assert_eq!(view.run_id, Some("current".into()));
                assert_eq!(view.refresh_policy, Some(InputRefreshPolicy::REFRESH));
                if expected == InputUpdateCardStatus::Queued {
                    state = state.reduce(PlaylistUpdateCardStatusAction::Progress {
                        connection_context: connection,
                        progress: PlaylistUpdateProgressEvent::for_run_input(
                            "current".into(),
                            1.into(),
                            7,
                            "input",
                            "running",
                        ),
                    });
                } else if expected == InputUpdateCardStatus::Updating {
                    state = state.reduce(PlaylistUpdateCardStatusAction::Progress {
                        connection_context: connection,
                        progress: PlaylistUpdateProgressEvent::input_completed(
                            "current".into(),
                            1.into(),
                            7,
                            PlaylistUpdateState::Success,
                            "input",
                            "completed",
                        ),
                    });
                }
            }
            state = state.reduce(PlaylistUpdateCardStatusAction::Completed {
                connection_context: connection,
                completed: PlaylistUpdateRunStateEvent::correlated(
                    "current".into(),
                    1.into(),
                    PlaylistUpdateState::Failure,
                ),
            });
            let view = build_input_update_run_views(&sources, &cards, &state, &persisted).remove(0);
            assert_eq!(view.input_state, InputUpdateCardStatus::Success);
            assert_eq!(view.overall_state, InputUpdateCardStatus::Failed);
            persisted.inputs[0].last_input_update =
                Some(PersistedPlaylistUpdateInputResult { state: PlaylistUpdateState::Success, timestamp: 456 });
            let reloaded = state
                .reduce(PlaylistUpdateCardStatusAction::ConnectionChanged(WebSocketConnectionContext::new(2, true)));
            let view = build_input_update_run_views(&sources, &cards, &reloaded, &persisted).remove(0);
            assert_eq!(view.input_state, InputUpdateCardStatus::Success);
            assert_eq!(view.run_id, None);
            assert!(view.progress_details.is_empty());
            assert_eq!(view.refresh_policy, None);
        }
    }

    #[test]
    fn pipeline_transparency_playlist_update_status_library_scan_stays_with_rebuilding_run_and_reload_is_unknown() {
        let input = input(17, "library", InputType::Library);
        let cards = vec![card(&input, &[], Some(1234))];
        let sources = SourcesConfigDto { inputs: vec![input], ..SourcesConfigDto::default() };
        let connection = WebSocketConnectionContext::new(1, true);
        let catalog = LibraryStatus { enabled: true, movies: 2, series: 3, episodes: 19, total_items: 5, path: None };
        let scan = LibraryScanResult {
            files_scanned: 12,
            groups_scanned: 7,
            files_added: 3,
            files_updated: 2,
            files_removed: 1,
            errors: 0,
        };
        let project = |state: &PlaylistUpdateCardStatuses| {
            build_input_update_run_views(&sources, &cards, state, &PlaylistUpdateStatusDto::default())
                .remove(0)
                .with_library_catalog(Some(&catalog))
        };
        let accepted = |run: &str| {
            PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: connection,
                run_id: run.into(),
                input_ids: vec![17],
                refresh_policy: InputRefreshPolicy::NORMAL,
            })
        };
        let progress =
            |event| PlaylistUpdateCardStatusAction::Progress { connection_context: connection, progress: event };
        let completed = |run: &str, order: u64, state| PlaylistUpdateCardStatusAction::Completed {
            connection_context: connection,
            completed: PlaylistUpdateRunStateEvent {
                run_id: Some(run.into()),
                execution_order: Some(order.into()),
                state,
            },
        };
        for terminal in [PlaylistUpdateState::Success, PlaylistUpdateState::Partial, PlaylistUpdateState::Failure] {
            let fresh = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection));
            let reload = project(&fresh);
            assert_eq!(reload.library_catalog, Some(catalog.clone()));
            assert_eq!(reload.library_scan_result, None);
            assert!(reload.cluster_results.is_empty());
            assert_eq!(reload.last_update_at, Some(1234));
            let state = fresh
                .reduce(accepted("r1"))
                .reduce(progress(
                    PlaylistUpdateProgressEvent::for_run_input("r1".into(), 1.into(), 17, "library", "scan finished")
                        .with_library_scan_result(scan.clone()),
                ))
                .reduce(progress(PlaylistUpdateProgressEvent::input_completed(
                    "r1".into(),
                    1.into(),
                    17,
                    PlaylistUpdateState::Success,
                    "library",
                    "input finished",
                )));
            let state = state.reduce(accepted("r2"));
            let rebuilding = project(&state);
            assert_eq!(rebuilding.run_id, Some("r1".into()));
            assert_eq!(rebuilding.input_state, InputUpdateCardStatus::Success);
            assert_eq!(rebuilding.overall_state, InputUpdateCardStatus::Updating);
            assert_eq!(rebuilding.library_scan_result, Some(scan.clone()));
            let state = state.reduce(completed("r1", 1_u64, terminal));
            let queued = project(&state);
            assert_eq!(queued.run_id, Some("r2".into()));
            assert_eq!(queued.overall_state, InputUpdateCardStatus::Queued);
            assert_eq!(queued.library_scan_result, None);
            assert!(queued.progress_details.is_empty());
            assert_eq!(project(&state.clone().reduce(accepted("r1"))), queued);
            let state = state.reduce(progress(PlaylistUpdateProgressEvent::for_run_input(
                "r2".into(),
                2.into(),
                17,
                "library",
                "scan started",
            )));
            assert_eq!(project(&state).library_scan_result, None);
            let failed_scan = LibraryScanResult { errors: 4, ..scan.clone() };
            let state = state
                .reduce(progress(
                    PlaylistUpdateProgressEvent::for_run_input("r2".into(), 2.into(), 17, "library", "scan result")
                        .with_library_scan_result(failed_scan.clone()),
                ))
                .reduce(progress(PlaylistUpdateProgressEvent::input_completed(
                    "r2".into(),
                    2.into(),
                    17,
                    PlaylistUpdateState::Failure,
                    "library",
                    "failed",
                )))
                .reduce(completed("r2", 2, PlaylistUpdateState::Failure));
            let failed = project(&state);
            assert_eq!(failed.library_scan_result, Some(failed_scan));
            assert_eq!(failed.overall_state, InputUpdateCardStatus::Failed);
            assert_eq!(failed.refresh_policy, Some(InputRefreshPolicy::NORMAL));
            assert_eq!(failed.library_catalog, Some(catalog.clone()));
            let reset = state
                .reduce(PlaylistUpdateCardStatusAction::ConnectionChanged(WebSocketConnectionContext::new(2, true)));
            assert_eq!(project(&reset).library_scan_result, None);
        }
    }

    #[test]
    fn pipeline_transparency_library_projection_does_not_replace_other_provider_facts() {
        for input_type in [InputType::Xtream, InputType::Stalker, InputType::M3u, InputType::Plex] {
            let input = input(7, "provider", input_type);
            let cards = vec![card(&input, &[], None)];
            let sources = SourcesConfigDto { inputs: vec![input], ..SourcesConfigDto::default() };
            let connection = WebSocketConnectionContext::new(1, true);
            let telemetry = PlaylistUpdateInputTelemetry {
                refresh_policy: InputRefreshPolicy::REFRESH,
                source: Some(PlaylistUpdateDataSource::Provider),
                clusters: vec![],
            };
            let state = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection)).reduce(
                PlaylistUpdateCardStatusAction::Progress {
                    connection_context: connection,
                    progress: PlaylistUpdateProgressEvent::for_run_input(
                        "provider".into(),
                        1.into(),
                        7,
                        "provider",
                        "loaded",
                    )
                    .with_input_telemetry(telemetry),
                },
            );
            let view = build_input_update_run_views(&sources, &cards, &state, &PlaylistUpdateStatusDto::default())
                .remove(0)
                .with_library_catalog(Some(&LibraryStatus { enabled: true, ..LibraryStatus::default() }));
            assert_eq!(view.cache_source, Some(PlaylistUpdateDataSource::Provider));
            assert_eq!(view.refresh_policy, Some(InputRefreshPolicy::REFRESH));
            assert_eq!(view.library_catalog, None);
            assert_eq!(view.library_scan_result, None);
        }
    }

    fn card(input: &ConfigInputDto, targets: &[(u16, &str)], last_update_at: Option<u64>) -> InputUpdateCardModel {
        InputUpdateCardModel {
            input_id: input.id,
            input_name: Arc::clone(&input.name),
            input_type: input.input_type,
            capabilities: InputUpdateCapabilities::for_input(input),
            targets: targets
                .iter()
                .map(|(id, name)| InputUpdateCardTarget { id: *id, name: (*name).to_string() })
                .collect(),
            last_update_at,
        }
    }

    fn target(id: u16, name: &str, processing_order: ProcessingOrder) -> ConfigTargetDto {
        ConfigTargetDto { id, name: name.to_string(), processing_order, ..ConfigTargetDto::default() }
    }

    fn rename() -> ConfigRenameDto {
        ConfigRenameDto {
            field: ItemField::Name,
            pattern: "old".to_string(),
            new_name: "new".to_string(),
            t_pattern: None,
        }
    }

    fn persisted_status(
        input_id: u16,
        clusters: &[(XtreamCluster, PersistedPlaylistUpdateClusterState, u64)],
    ) -> PlaylistUpdateStatusDto {
        PlaylistUpdateStatusDto {
            inputs: vec![InputPlaylistUpdateStatusDto {
                input_id,
                last_update: clusters.iter().map(|(_, _, timestamp)| *timestamp).max(),
                last_input_update: None,
                clusters: clusters
                    .iter()
                    .map(|(cluster, status, timestamp)| PersistedPlaylistUpdateClusterStatusDto {
                        cluster: *cluster,
                        status: *status,
                        timestamp: *timestamp,
                        last_update: None,
                    })
                    .collect(),
            }],
            ..PlaylistUpdateStatusDto::default()
        }
    }

    fn statuses_with_manual_multi_input_run() -> Rc<PlaylistUpdateCardStatuses> {
        let connection = WebSocketConnectionContext::new(4, true);
        Rc::new(PlaylistUpdateCardStatuses::for_connection(connection))
            .reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: connection,
                run_id: "run-stable".into(),
                input_ids: vec![7],
                refresh_policy: InputRefreshPolicy::FORCE,
            }))
            .reduce(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    "run-stable".into(),
                    PlaylistUpdateRunOrder::from(17),
                    7,
                    "display-name",
                    "accepted input progress",
                ),
            })
            .reduce(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    "run-stable".into(),
                    PlaylistUpdateRunOrder::from(17),
                    9,
                    "same-display-name",
                    "observed dependency progress",
                ),
            })
    }

    #[test]
    fn persisted_cluster_status_restores_reload_fallback_without_runtime_inference() {
        let mut xtream = input(7, "xtream", InputType::Xtream);
        xtream.options = Some(ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, vod: 80, series: 100 },
            ..ConfigInputOptionsDto::default()
        });
        let m3u = input(8, "m3u", InputType::M3u);
        let sources = SourcesConfigDto {
            inputs: vec![xtream.clone(), m3u.clone()],
            sources: Vec::new(),
            ..SourcesConfigDto::default()
        };
        let cards = vec![card(&xtream, &[], Some(303)), card(&m3u, &[], Some(404))];
        let persisted = persisted_status(
            7,
            &[
                (XtreamCluster::Live, PersistedPlaylistUpdateClusterState::Ok, 101),
                (XtreamCluster::Video, PersistedPlaylistUpdateClusterState::Failed, 202),
                (XtreamCluster::Series, PersistedPlaylistUpdateClusterState::Ok, 303),
            ],
        );

        let views = build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted);

        assert_eq!(views[0].run_id, None);
        assert_eq!(
            views[0]
                .cluster_results
                .iter()
                .map(|cluster| cluster.persisted_status.map(|status| status.status))
                .collect::<Vec<_>>(),
            [
                Some(PersistedPlaylistUpdateClusterState::Ok),
                Some(PersistedPlaylistUpdateClusterState::Failed),
                Some(PersistedPlaylistUpdateClusterState::Ok),
            ]
        );
        assert_eq!(
            views[0]
                .cluster_results
                .iter()
                .map(|cluster| cluster.persisted_status.map(|status| status.timestamp))
                .collect::<Vec<_>>(),
            [Some(101), Some(202), Some(303)]
        );
        assert_eq!(
            views[0].cluster_results.iter().map(|cluster| cluster.threshold).collect::<Vec<_>>(),
            [Some(90), Some(80), Some(100)]
        );
        assert!(views[0].cluster_results.iter().all(|cluster| {
            cluster.requested.is_none()
                && cluster.baseline_count.is_none()
                && cluster.candidate_count.is_none()
                && cluster.active_count.is_none()
                && cluster.quality.is_none()
                && cluster.source.is_none()
                && cluster.outcome.is_none()
        }));
        assert!(views[1].cluster_results.is_empty(), "M3U must not receive persisted synthetic clusters");
    }

    #[test]
    fn persisted_cluster_status_snapshot_restores_typed_facts_and_keeps_technical_failure_separate() {
        let mut xtream = input(7, "xtream", InputType::Xtream);
        xtream.options = Some(ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 80, vod: 80, series: 80 },
            ..ConfigInputOptionsDto::default()
        });
        let sources =
            SourcesConfigDto { inputs: vec![xtream.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&xtream, &[], Some(303))];
        let accepted_quality = PersistedPlaylistUpdateQualitySnapshot {
            threshold: 90,
            baseline_count: Some(1_000),
            candidate_count: Some(950),
            achieved_quality: Some(95),
            decision: PersistedPlaylistUpdateQualityDecision::Accepted,
        };
        let persisted = PlaylistUpdateStatusDto {
            inputs: vec![InputPlaylistUpdateStatusDto {
                input_id: 7,
                last_update: Some(303),
                last_input_update: None,
                clusters: vec![
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Live,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 101,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: Some(InputRefreshPolicy::REFRESH),
                            source: Some(PlaylistUpdateDataSource::Provider),
                            quality_guard_threshold: None,
                            quality: Some(accepted_quality),
                            active_count: Some(950),
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
                        }),
                    },
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Video,
                        status: PersistedPlaylistUpdateClusterState::Failed,
                        timestamp: 202,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: None,
                            source: Some(PlaylistUpdateDataSource::Provider),
                            quality_guard_threshold: None,
                            quality: Some(PersistedPlaylistUpdateQualitySnapshot {
                                threshold: 95,
                                baseline_count: Some(500),
                                candidate_count: Some(300),
                                achieved_quality: Some(60),
                                decision: PersistedPlaylistUpdateQualityDecision::Rejected,
                            }),
                            active_count: Some(500),
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
                        }),
                    },
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Series,
                        status: PersistedPlaylistUpdateClusterState::Failed,
                        timestamp: 303,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: None,
                            source: Some(PlaylistUpdateDataSource::Provider),
                            quality_guard_threshold: None,
                            quality: Some(accepted_quality),
                            active_count: None,
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Failed),
                        }),
                    },
                ],
            }],
            ..PlaylistUpdateStatusDto::default()
        };

        let views = build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted);
        let live = views[0].cluster_results[0];
        assert_eq!(live.refresh_policy, Some(InputRefreshPolicy::REFRESH));
        assert_eq!(live.source, Some(PlaylistUpdateDataSource::Provider));
        assert_eq!(live.outcome, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!((live.threshold, live.quality), (Some(90), Some(95)));
        assert_eq!((live.baseline_count, live.candidate_count, live.active_count), (Some(1_000), Some(950), Some(950)));
        assert_eq!(live.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));

        let video = views[0].cluster_results[1];
        assert_eq!(video.outcome, Some(PlaylistUpdateClusterDecision::Rejected));
        assert_eq!(video.active_count, Some(500));
        assert_eq!(video.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Succeeded));

        let series = views[0].cluster_results[2];
        assert_eq!(series.outcome, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!(series.technical_state, Some(PersistedPlaylistUpdateTechnicalState::Failed));
        assert_eq!(series.active_count, None);
    }

    #[test]
    fn persisted_cluster_status_snapshot_keeps_bootstrap_cache_and_disabled_force_facts_sparse() {
        let mut xtream = input(7, "xtream", InputType::Xtream);
        xtream.options = Some(ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, vod: 90, series: 0 },
            ..ConfigInputOptionsDto::default()
        });
        let sources =
            SourcesConfigDto { inputs: vec![xtream.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&xtream, &[], Some(303))];
        let persisted = PlaylistUpdateStatusDto {
            inputs: vec![InputPlaylistUpdateStatusDto {
                input_id: 7,
                last_update: Some(303),
                last_input_update: None,
                clusters: vec![
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Live,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 101,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: None,
                            source: Some(PlaylistUpdateDataSource::Provider),
                            quality_guard_threshold: None,
                            quality: Some(PersistedPlaylistUpdateQualitySnapshot {
                                threshold: 90,
                                baseline_count: None,
                                candidate_count: Some(42),
                                achieved_quality: None,
                                decision: PersistedPlaylistUpdateQualityDecision::Accepted,
                            }),
                            active_count: Some(42),
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
                        }),
                    },
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Video,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 202,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: Some(InputRefreshPolicy::REFRESH),
                            source: Some(PlaylistUpdateDataSource::Cache),
                            quality_guard_threshold: Some(90),
                            quality: None,
                            active_count: None,
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
                        }),
                    },
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Series,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 303,
                        last_update: Some(PersistedPlaylistUpdateClusterSnapshot {
                            policy: Some(InputRefreshPolicy::FORCE),
                            source: Some(PlaylistUpdateDataSource::Provider),
                            quality_guard_threshold: Some(0),
                            quality: None,
                            active_count: Some(12),
                            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
                        }),
                    },
                ],
            }],
            ..PlaylistUpdateStatusDto::default()
        };

        let views = build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted);
        let live = views[0].cluster_results[0];
        assert_eq!(live.outcome, Some(PlaylistUpdateClusterDecision::Accepted));
        assert_eq!((live.baseline_count, live.candidate_count), (None, Some(42)));
        assert_eq!((live.threshold, live.quality), (Some(90), None));
        assert_eq!(live.refresh_policy, None, "an automatic run must not acquire a default policy");

        let video = views[0].cluster_results[1];
        assert_eq!(video.source, Some(PlaylistUpdateDataSource::Cache));
        assert_eq!(video.refresh_policy, Some(InputRefreshPolicy::REFRESH));
        assert_eq!(video.outcome, None);

        let series = views[0].cluster_results[2];
        assert_eq!(series.refresh_policy, Some(InputRefreshPolicy::FORCE));
        assert_eq!(series.threshold, None);
        assert_eq!(series.quality, None);
        assert_eq!(series.active_count, Some(12));
    }

    #[test]
    fn persisted_cluster_status_snapshot_uses_historical_guard_before_current_configuration() {
        let mut xtream = input(7, "xtream", InputType::Xtream);
        xtream.options = Some(ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 0, vod: 95, series: 80 },
            ..ConfigInputOptionsDto::default()
        });
        let sources =
            SourcesConfigDto { inputs: vec![xtream.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&xtream, &[], Some(303))];
        let force_snapshot = |quality_guard_threshold| PersistedPlaylistUpdateClusterSnapshot {
            policy: Some(InputRefreshPolicy::FORCE),
            source: Some(PlaylistUpdateDataSource::Provider),
            quality_guard_threshold,
            quality: None,
            active_count: Some(42),
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
        };
        let persisted = PlaylistUpdateStatusDto {
            inputs: vec![InputPlaylistUpdateStatusDto {
                input_id: 7,
                last_update: Some(303),
                last_input_update: None,
                clusters: vec![
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Live,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 101,
                        last_update: Some(force_snapshot(Some(95))),
                    },
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Video,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 202,
                        last_update: Some(force_snapshot(Some(0))),
                    },
                    PersistedPlaylistUpdateClusterStatusDto {
                        cluster: XtreamCluster::Series,
                        status: PersistedPlaylistUpdateClusterState::Ok,
                        timestamp: 303,
                        last_update: Some(force_snapshot(None)),
                    },
                ],
            }],
            ..PlaylistUpdateStatusDto::default()
        };

        let views = build_input_update_run_views(&sources, &cards, &PlaylistUpdateCardStatuses::default(), &persisted);
        assert_eq!(views[0].cluster_results[0].threshold, Some(95), "historical active guard beats disabled config");
        assert_eq!(views[0].cluster_results[1].threshold, None, "historical disabled guard beats active config");
        assert_eq!(
            views[0].cluster_results[2].threshold,
            Some(80),
            "only a legacy Block-22a snapshot falls back to current configuration"
        );
    }

    #[test]
    fn persisted_cluster_status_missing_entry_remains_unknown() {
        let xtream = input(7, "xtream", InputType::Xtream);
        let sources =
            SourcesConfigDto { inputs: vec![xtream.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&xtream, &[], None)];

        let views = build_input_update_run_views(
            &sources,
            &cards,
            &PlaylistUpdateCardStatuses::default(),
            &PlaylistUpdateStatusDto::default(),
        );

        assert!(views[0].cluster_results.iter().all(|cluster| cluster.persisted_status.is_none()));
    }

    #[test]
    fn persisted_cluster_status_never_augments_queued_or_active_run() {
        let xtream = input(7, "xtream", InputType::Xtream);
        let sources =
            SourcesConfigDto { inputs: vec![xtream.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&xtream, &[], Some(101))];
        let mut persisted =
            persisted_status(7, &[(XtreamCluster::Live, PersistedPlaylistUpdateClusterState::Failed, 101)]);
        persisted.inputs[0].clusters[0].last_update = Some(PersistedPlaylistUpdateClusterSnapshot {
            policy: Some(InputRefreshPolicy::FORCE),
            source: Some(PlaylistUpdateDataSource::Provider),
            quality_guard_threshold: Some(95),
            quality: None,
            active_count: Some(100),
            technical_state: Some(PersistedPlaylistUpdateTechnicalState::Succeeded),
        });
        let connection = WebSocketConnectionContext::new(10, true);
        let queued = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection)).reduce(
            PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: connection,
                run_id: "new-run".into(),
                input_ids: vec![7],
                refresh_policy: InputRefreshPolicy::NORMAL,
            }),
        );

        let queued_views = build_input_update_run_views(&sources, &cards, &queued, &persisted);
        assert_eq!(queued_views[0].overall_state, InputUpdateCardStatus::Queued);
        assert!(queued_views[0].cluster_results.iter().all(|cluster| {
            cluster.persisted_status.is_none()
                && cluster.outcome.is_none()
                && cluster.source.is_none()
                && cluster.quality.is_none()
                && cluster.threshold.is_none()
                && cluster.refresh_policy.is_none()
                && cluster.technical_state.is_none()
        }));

        let active = queued.reduce(PlaylistUpdateCardStatusAction::Progress {
            connection_context: connection,
            progress: PlaylistUpdateProgressEvent::for_run_input(
                "new-run".into(),
                PlaylistUpdateRunOrder::from(1),
                7,
                "xtream",
                "updating",
            ),
        });
        let active_views = build_input_update_run_views(&sources, &cards, &active, &persisted);
        assert_eq!(active_views[0].overall_state, InputUpdateCardStatus::Updating);
        assert!(active_views[0].cluster_results.iter().all(|cluster| cluster.persisted_status.is_none()));
    }

    #[test]
    fn persisted_cluster_status_reload_is_adopted_after_current_run_lifecycle_resets() {
        let xtream = input(7, "xtream", InputType::Xtream);
        let sources =
            SourcesConfigDto { inputs: vec![xtream.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&xtream, &[], Some(202))];
        let refreshed = persisted_status(7, &[(XtreamCluster::Live, PersistedPlaylistUpdateClusterState::Failed, 202)]);
        let connection = WebSocketConnectionContext::new(11, true);
        let telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Live,
                requested: true,
                source: Some(InputDataSourceView::Provider),
                baseline_count: Some(100),
                candidate_count: Some(100),
                active_count: Some(100),
                threshold: Some(80),
                quality: Some(100),
                decision: Some(InputClusterOutcomeView::Accepted),
                technical_state: None,
            }],
        };
        let current = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection))
            .reduce(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::input_completed(
                    "current-run".into(),
                    PlaylistUpdateRunOrder::from(1),
                    7,
                    PlaylistUpdateState::Success,
                    "xtream",
                    "completed",
                )
                .with_input_telemetry(telemetry),
            })
            .reduce(PlaylistUpdateCardStatusAction::Completed {
                connection_context: connection,
                completed: PlaylistUpdateRunStateEvent::correlated(
                    "current-run".into(),
                    PlaylistUpdateRunOrder::from(1),
                    PlaylistUpdateState::Success,
                ),
            });

        let current_views = build_input_update_run_views(&sources, &cards, &current, &refreshed);
        let current_live = current_views[0].cluster_results[0];
        assert_eq!(current_live.outcome, Some(InputClusterOutcomeView::Accepted));
        assert_eq!(current_live.quality, Some(100));
        assert_eq!(current_live.persisted_status, None);

        let reset = current
            .reduce(PlaylistUpdateCardStatusAction::ConnectionChanged(WebSocketConnectionContext::new(12, true)));
        let reloaded_views = build_input_update_run_views(&sources, &cards, &reset, &refreshed);
        let reloaded_live = reloaded_views[0].cluster_results[0];
        assert_eq!(reloaded_live.outcome, None);
        assert_eq!(reloaded_live.quality, None);
        assert_eq!(
            reloaded_live.persisted_status,
            Some(PersistedClusterStatusView {
                status: PersistedPlaylistUpdateClusterState::Failed,
                timestamp: 202,
                last_update: None,
            })
        );
    }

    #[test]
    fn pipeline_transparency_model_aggregates_stable_ids_and_preserves_typed_refresh_policy() {
        let input_a = input(7, "same-name", InputType::Xtream);
        let input_b = input(9, "same-name", InputType::M3u);
        let target_a = target(41, "same-target-name", ProcessingOrder::Rmf);
        let target_b = target(42, "same-target-name", ProcessingOrder::Frm);
        let sources = SourcesConfigDto {
            inputs: vec![input_a.clone(), input_b.clone()],
            sources: vec![ConfigSourceDto { inputs: Vec::new(), targets: vec![target_a, target_b] }],
            ..SourcesConfigDto::default()
        };
        let cards = vec![
            card(&input_a, &[(41, "same-target-name"), (42, "same-target-name")], Some(500)),
            card(&input_b, &[(41, "same-target-name")], None),
        ];

        let views = build_input_update_run_views(
            &sources,
            &cards,
            &statuses_with_manual_multi_input_run(),
            &PlaylistUpdateStatusDto::default(),
        );

        assert_eq!(views.iter().map(|view| view.input_id).collect::<Vec<_>>(), [7, 9]);
        assert_eq!(views[0].run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("run-stable"));
        assert_eq!(views[0].execution_order.map(PlaylistUpdateRunOrder::get), Some(17));
        assert_eq!(views[0].overall_state, InputUpdateCardStatus::Updating);
        assert_eq!(views[0].refresh_policy, Some(InputRefreshPolicy::FORCE));
        assert_eq!(views[0].cache_source, None);
        assert!(views[0].refresh_policy.is_some_and(InputRefreshPolicy::bypasses_cache));
        assert!(views[0].refresh_policy.is_some_and(InputRefreshPolicy::bypasses_quality));
        assert_eq!(views[0].progress_details, ["accepted input progress"]);
        assert_eq!(views[0].target_results.iter().map(|target| target.target_id).collect::<Vec<_>>(), [41, 42]);
        assert_eq!(views[0].target_results[0].input_ids, [7, 9]);
        assert_eq!(views[0].target_results[1].input_ids, [7]);
        assert_eq!(views[1].run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("run-stable"));
        assert_eq!(views[1].execution_order.map(PlaylistUpdateRunOrder::get), Some(17));
        assert_eq!(views[1].overall_state, InputUpdateCardStatus::Updating);
        assert_eq!(views[1].refresh_policy, None);
        assert_eq!(views[1].progress_details, ["observed dependency progress"]);
        assert_eq!(views[1].last_update_at, None);
    }

    #[test]
    fn pipeline_transparency_model_maps_cluster_configuration_and_quality_without_redeciding() {
        let mut xtream = input(7, "xtream", InputType::Xtream);
        xtream.options = Some(ConfigInputOptionsDto {
            skip_vod: true,
            update_quality: ConfigInputUpdateQualityDto { live: 90, vod: 95, series: 100 },
            ..ConfigInputOptionsDto::default()
        });
        let stalker = input(9, "stalker", InputType::Stalker);
        let m3u = input(8, "m3u", InputType::M3u);
        let sources = SourcesConfigDto {
            inputs: vec![xtream.clone(), stalker.clone(), m3u.clone()],
            sources: Vec::new(),
            ..SourcesConfigDto::default()
        };
        let cards = vec![card(&xtream, &[], None), card(&stalker, &[], None), card(&m3u, &[], None)];

        let views = build_input_update_run_views(
            &sources,
            &cards,
            &PlaylistUpdateCardStatuses::default(),
            &PlaylistUpdateStatusDto::default(),
        );

        assert_eq!(
            views[0].cluster_results.iter().map(|view| view.cluster).collect::<Vec<_>>(),
            [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series]
        );
        assert_eq!(
            views[0].cluster_results.iter().map(|view| view.enabled_by_configuration).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert_eq!(
            views[0].cluster_results.iter().map(|view| view.threshold).collect::<Vec<_>>(),
            [Some(90), Some(95), Some(100)]
        );
        assert!(views[0].cluster_results.iter().all(|view| {
            view.baseline_count.is_none()
                && view.candidate_count.is_none()
                && view.active_count.is_none()
                && view.quality.is_none()
                && view.source.is_none()
                && view.outcome.is_none()
        }));
        assert_eq!(
            views[1].cluster_results.iter().map(|view| view.cluster).collect::<Vec<_>>(),
            [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series]
        );
        assert!(views[2].cluster_results.is_empty(), "M3U must not receive synthetic clusters");
    }

    #[test]
    fn pipeline_transparency_target_uses_real_orders_and_only_configured_rule_counts() {
        let input = input(7, "provider", InputType::Stalker);
        let mut configured_target = target(41, "target", ProcessingOrder::Mrf);
        configured_target.filter = ConfigTargetFilterDto {
            processing: Some("Group ~ .*".to_string()),
            persist: Some("EpgId ~ .+".to_string()),
            ..ConfigTargetFilterDto::default()
        };
        configured_target.rename = Some(vec![rename(), rename()]);
        configured_target.mapping = Some(vec!["first".to_string(), "second".to_string(), "third".to_string()]);
        let mut target_without_rules = target(42, "empty-target", ProcessingOrder::Fmr);
        target_without_rules.filter.processing = Some("  ".to_string());
        let sources = SourcesConfigDto {
            inputs: vec![input.clone()],
            sources: vec![ConfigSourceDto {
                inputs: Vec::new(),
                targets: vec![configured_target, target_without_rules],
            }],
            ..SourcesConfigDto::default()
        };
        let cards = vec![card(&input, &[(41, "target"), (42, "empty-target")], None)];

        let views = build_input_update_run_views(
            &sources,
            &cards,
            &PlaylistUpdateCardStatuses::default(),
            &PlaylistUpdateStatusDto::default(),
        );
        let configured = views[0].target_results[0].pipeline.expect("configured target pipeline");
        let empty = views[0].target_results[1].pipeline.expect("empty configured target pipeline");

        assert_eq!(configured.processing_order, ProcessingOrder::Mrf);
        assert_eq!(configured.filter_summary.configured_count, 2);
        assert_eq!(configured.rename_summary.configured_count, 2);
        assert_eq!(configured.mapping_summary.configured_count, 3);
        assert_eq!(empty.processing_order, ProcessingOrder::Fmr);
        assert_eq!(empty.filter_summary.configured_count, 0);
        assert_eq!(empty.rename_summary.configured_count, 0);
        assert_eq!(empty.mapping_summary.configured_count, 0);
    }

    #[test]
    fn pipeline_transparency_model_keeps_unavailable_runtime_values_optional() {
        let input = input(7, "provider", InputType::Xtream);
        let sources =
            SourcesConfigDto { inputs: vec![input.clone()], sources: Vec::new(), ..SourcesConfigDto::default() };
        let cards = vec![card(&input, &[(99, "removed-target")], None)];

        let views = build_input_update_run_views(
            &sources,
            &cards,
            &PlaylistUpdateCardStatuses::default(),
            &PlaylistUpdateStatusDto::default(),
        );
        let view = &views[0];

        assert_eq!(view.run_id, None);
        assert_eq!(view.execution_order, None);
        assert_eq!(view.overall_state, InputUpdateCardStatus::Ready);
        assert_eq!(view.refresh_policy, None);
        assert_eq!(view.cache_source, None);
        assert_eq!(view.last_update_at, None);
        assert!(view.progress_details.is_empty());
        assert!(view.cluster_results.iter().all(|cluster| cluster.threshold.is_none()));
        assert_eq!(view.target_results[0].target_id, 99);
        assert_eq!(view.target_results[0].pipeline, None);
    }

    #[test]
    fn pipeline_transparency_input_keeps_typed_source_and_outcome_without_redeciding() {
        let xtream = input(7, "xtream", InputType::Xtream);
        let m3u = input(8, "m3u", InputType::M3u);
        let sources = SourcesConfigDto {
            inputs: vec![xtream.clone(), m3u.clone()],
            sources: Vec::new(),
            ..SourcesConfigDto::default()
        };
        let cards = vec![card(&xtream, &[], None), card(&m3u, &[], None)];
        let connection = WebSocketConnectionContext::new(8, true);
        let statuses = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection))
            .reduce(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    "runtime-telemetry".into(),
                    PlaylistUpdateRunOrder::from(23),
                    7,
                    "xtream",
                    "typed runtime facts",
                )
                .with_input_telemetry(PlaylistUpdateInputTelemetry {
                    refresh_policy: InputRefreshPolicy::NORMAL,
                    source: None,
                    clusters: vec![PlaylistUpdateClusterTelemetry {
                        cluster: XtreamCluster::Live,
                        requested: true,
                        source: Some(InputDataSourceView::Provider),
                        baseline_count: Some(12_543),
                        candidate_count: Some(217),
                        active_count: Some(12_543),
                        threshold: Some(90),
                        quality: Some(1),
                        decision: Some(InputClusterOutcomeView::Rejected),
                        technical_state: None,
                    }],
                }),
            })
            .reduce(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::for_run_input(
                    "runtime-telemetry".into(),
                    PlaylistUpdateRunOrder::from(23),
                    8,
                    "m3u",
                    "typed cache source",
                )
                .with_input_telemetry(PlaylistUpdateInputTelemetry {
                    refresh_policy: InputRefreshPolicy::NORMAL,
                    source: Some(InputDataSourceView::Cache),
                    clusters: Vec::new(),
                }),
            });

        let views = build_input_update_run_views(&sources, &cards, &statuses, &PlaylistUpdateStatusDto::default());

        let cluster = views[0].cluster_results[0];
        assert_eq!(views[0].refresh_policy, Some(InputRefreshPolicy::NORMAL));
        assert_eq!(cluster.requested, Some(true));
        assert_eq!(cluster.source, Some(InputDataSourceView::Provider));
        assert_eq!(cluster.outcome, Some(InputClusterOutcomeView::Rejected));
        assert_eq!((cluster.threshold, cluster.quality), (Some(90), Some(1)));
        assert_eq!(
            (cluster.baseline_count, cluster.candidate_count, cluster.active_count),
            (Some(12_543), Some(217), Some(12_543))
        );
        assert_eq!(views[1].cache_source, Some(InputDataSourceView::Cache));
        assert!(views[1].cluster_results.is_empty(), "M3U must remain a single input pipeline");
    }

    #[test]
    fn pipeline_transparency_integration_keeps_partial_target_and_follow_up_run_facts_separate() {
        let mut xtream = input(7, "provider", InputType::Xtream);
        xtream.options = Some(ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, vod: 0, series: 100 },
            ..ConfigInputOptionsDto::default()
        });
        let mut configured_target = target(41, "output", ProcessingOrder::Rmf);
        configured_target.filter.processing = Some("Group ~ .*".to_string());
        configured_target.rename = Some(vec![rename()]);
        configured_target.mapping = Some(vec!["mapping".to_string()]);
        let sources = SourcesConfigDto {
            inputs: vec![xtream.clone()],
            sources: vec![ConfigSourceDto { inputs: Vec::new(), targets: vec![configured_target] }],
            ..SourcesConfigDto::default()
        };
        let cards = vec![card(&xtream, &[(41, "output")], Some(500))];
        let connection = WebSocketConnectionContext::new(9, true);
        let partial_telemetry = PlaylistUpdateInputTelemetry {
            refresh_policy: InputRefreshPolicy::NORMAL,
            source: None,
            clusters: vec![PlaylistUpdateClusterTelemetry {
                cluster: XtreamCluster::Live,
                requested: true,
                source: Some(InputDataSourceView::Provider),
                baseline_count: Some(100),
                candidate_count: Some(80),
                active_count: Some(100),
                threshold: Some(90),
                quality: Some(80),
                decision: Some(InputClusterOutcomeView::Rejected),
                technical_state: None,
            }],
        };
        let statuses = Rc::new(PlaylistUpdateCardStatuses::for_connection(connection))
            .reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
                connection_context: connection,
                run_id: "partial-run".into(),
                input_ids: vec![7],
                refresh_policy: InputRefreshPolicy::NORMAL,
            }))
            .reduce(PlaylistUpdateCardStatusAction::Progress {
                connection_context: connection,
                progress: PlaylistUpdateProgressEvent::input_completed(
                    "partial-run".into(),
                    PlaylistUpdateRunOrder::from(1),
                    7,
                    PlaylistUpdateState::Partial,
                    "provider",
                    "live quality rejected",
                )
                .with_input_telemetry(partial_telemetry),
            })
            .reduce(PlaylistUpdateCardStatusAction::Completed {
                connection_context: connection,
                completed: PlaylistUpdateRunStateEvent::correlated(
                    "partial-run".into(),
                    PlaylistUpdateRunOrder::from(1),
                    PlaylistUpdateState::Partial,
                ),
            });

        let partial_views =
            build_input_update_run_views(&sources, &cards, &statuses, &PlaylistUpdateStatusDto::default());
        let partial = &partial_views[0];
        assert_eq!(partial.overall_state, InputUpdateCardStatus::Partial);
        assert_eq!(partial.progress_details, ["live quality rejected"]);
        let live = partial.cluster_results[0];
        assert_eq!(live.outcome, Some(InputClusterOutcomeView::Rejected));
        assert_eq!(live.source, Some(InputDataSourceView::Provider));
        assert_eq!((live.threshold, live.quality), (Some(90), Some(80)));
        let target = &partial.target_results[0];
        let pipeline = target.pipeline.expect("configured target pipeline");
        assert_eq!(target.target_id, 41);
        assert_eq!(target.input_ids, [7]);
        assert_eq!(pipeline.processing_order, ProcessingOrder::Rmf);
        assert_eq!(pipeline.filter_summary.configured_count, 1);
        assert_eq!(pipeline.rename_summary.configured_count, 1);
        assert_eq!(pipeline.mapping_summary.configured_count, 1);

        let statuses = statuses.reduce(PlaylistUpdateCardStatusAction::Accepted(PlaylistUpdateAcceptedScope {
            connection_context: connection,
            run_id: "force-run".into(),
            input_ids: vec![7],
            refresh_policy: InputRefreshPolicy::FORCE,
        }));
        let queued_views =
            build_input_update_run_views(&sources, &cards, &statuses, &PlaylistUpdateStatusDto::default());
        let queued = &queued_views[0];

        assert_eq!(queued.run_id.as_ref().map(PlaylistUpdateRunId::as_ref), Some("force-run"));
        assert_eq!(queued.overall_state, InputUpdateCardStatus::Queued);
        assert_eq!(queued.refresh_policy, Some(InputRefreshPolicy::FORCE));
        assert!(queued.progress_details.is_empty());
        assert_eq!(queued.target_results, partial.target_results);
        let live = queued.cluster_results[0];
        assert_eq!(live.requested, None);
        assert_eq!(live.source, None);
        assert_eq!(live.outcome, None);
        assert_eq!((live.baseline_count, live.candidate_count, live.active_count), (None, None, None));
        assert_eq!((live.threshold, live.quality), (Some(90), None));
    }
}
