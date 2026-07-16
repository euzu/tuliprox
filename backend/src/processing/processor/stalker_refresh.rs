use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use shared::error::TuliproxError;
use shared::model::stalker::StalkerStreamKind;
use shared::model::stalker_item::StalkerPlaylistItem;

use crate::model::{AppConfig, ConfigInput, ConfigInputFlags};
use crate::processing::parser::stalker as parser;
use crate::repository::stalker_generation_repository::{
    cleanup_obsolete_generations, clear_checkpoint, generation_data_path, load_active_manifest,
    load_checkpoint, publish_cluster, save_checkpoint, ClusterFiles, PublishedCluster, SeriesFiles,
    StalkerCheckpoint, StalkerGenerationData, StalkerRefreshPhase,
};
use crate::repository::stalker_repository::{
    load_stalker_items_after, prepare_stalker_episode_series_at, promote_stalker_file,
    remove_stalker_file, snapshot_stalker_epg_at, snapshot_stalker_items_at,
    upsert_stalker_epg_at, upsert_stalker_items_at,
};
use crate::utils::network::stalker::catalog::{StalkerCategory, StalkerRawItem};
use crate::utils::network::stalker::client::StalkerApiClient;
use crate::utils::network::stalker::error::StalkerError;
use crate::utils::network::stalker::profile::StalkerHandshake;
use super::stalker::StalkerCluster;

const MAX_RETRIES: u8 = 3;
const SKIPPED_SAMPLE_LIMIT: usize = 32;

pub enum StalkerRefreshLimit {
    Deadline(Instant),
    #[cfg(test)]
    Units { remaining: usize },
    Unlimited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StalkerRefreshMode {
    ServerSlice,
    Complete,
}

impl StalkerRefreshMode {
    pub fn budget(self) -> StalkerRefreshBudget {
        match self {
            Self::ServerSlice => {
                StalkerRefreshBudget::deadline(Instant::now() + std::time::Duration::from_mins(45))
            }
            Self::Complete => StalkerRefreshBudget::unlimited(),
        }
    }
}

pub struct StalkerRefreshBudget {
    limit: StalkerRefreshLimit,
}

impl StalkerRefreshBudget {
    pub fn deadline(deadline: Instant) -> Self { Self { limit: StalkerRefreshLimit::Deadline(deadline) } }

    pub fn unlimited() -> Self { Self { limit: StalkerRefreshLimit::Unlimited } }

    #[cfg(test)]
    fn units(remaining: usize) -> Self { Self { limit: StalkerRefreshLimit::Units { remaining } } }

    fn completed_unit(&mut self) {
        let _ = &mut self.limit;
        #[cfg(test)]
        if let StalkerRefreshLimit::Units { remaining } = &mut self.limit {
            *remaining = remaining.saturating_sub(1);
        }
    }

    fn should_yield(&self) -> bool {
        match self.limit {
            StalkerRefreshLimit::Deadline(deadline) => Instant::now() >= deadline,
            #[cfg(test)]
            StalkerRefreshLimit::Units { remaining } => remaining == 0,
            StalkerRefreshLimit::Unlimited => false,
        }
    }
}

#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct StalkerClusterSelection {
    live: bool,
    vod: bool,
    series: bool,
    epg: bool,
}

impl From<&ConfigInput> for StalkerClusterSelection {
    fn from(input: &ConfigInput) -> Self {
        let live = !input.has_flag(ConfigInputFlags::SkipLive);
        Self {
            live,
            vod: !input.has_flag(ConfigInputFlags::SkipVod),
            series: !input.has_flag(ConfigInputFlags::SkipSeries),
            epg: live && input.has_flag(ConfigInputFlags::StalkerBulkEpg),
        }
    }
}

impl StalkerClusterSelection {
    pub fn requested(input: &ConfigInput, requested: &[StalkerCluster]) -> Self {
        let configured = Self::from(input);
        let live = configured.live && requested.contains(&StalkerCluster::Live);
        Self {
            live,
            vod: configured.vod && requested.contains(&StalkerCluster::Vod),
            series: configured.series && requested.contains(&StalkerCluster::Series),
            epg: configured.epg && live,
        }
    }

    fn mask(self) -> u8 {
        u8::from(self.live)
            | (u8::from(self.vod) << 1)
            | (u8::from(self.series) << 2)
            | (u8::from(self.epg) << 3)
    }
}

fn first_phase(selection: StalkerClusterSelection) -> StalkerRefreshPhase {
    if selection.live {
        StalkerRefreshPhase::LiveBulk
    } else {
        next_phase_after_live(selection)
    }
}

fn next_phase_after_live(selection: StalkerClusterSelection) -> StalkerRefreshPhase {
    if selection.vod {
        StalkerRefreshPhase::Vod { page: 1 }
    } else {
        next_phase_after_vod(selection)
    }
}

fn next_phase_after_vod(selection: StalkerClusterSelection) -> StalkerRefreshPhase {
    if selection.series {
        StalkerRefreshPhase::SeriesRoots { page: 1 }
    } else if selection.epg {
        StalkerRefreshPhase::Epg
    } else {
        StalkerRefreshPhase::Complete
    }
}

fn next_phase_after_series(selection: StalkerClusterSelection) -> StalkerRefreshPhase {
    if selection.epg { StalkerRefreshPhase::Epg } else { StalkerRefreshPhase::Complete }
}

#[derive(Debug)]
pub enum StalkerRefreshOutcome {
    Complete,
    Yielded {
        phase: StalkerRefreshPhase,
        processed: u64,
        skipped: u64,
        error: Option<TuliproxError>,
    },
    Terminal(TuliproxError),
}

fn generation_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
}

fn category_map(categories: Vec<StalkerCategory>) -> HashMap<u32, StalkerCategory> {
    categories
        .into_iter()
        .filter_map(|category| category.id.parse().ok().map(|id| (id, category)))
        .collect()
}

fn category_map_result(
    result: Result<Vec<StalkerCategory>, StalkerError>,
) -> Result<HashMap<u32, StalkerCategory>, TuliproxError> {
    result.map(category_map).map_err(|err| provider_error(&err))
}

fn map_items(
    raw_items: &[StalkerRawItem],
    categories: &HashMap<u32, StalkerCategory>,
    kind: StalkerStreamKind,
    added_at: i64,
) -> Vec<StalkerPlaylistItem> {
    raw_items
        .iter()
        .map(|raw| {
            let category = raw
                .category_id
                .as_deref()
                .and_then(|value| value.parse::<u32>().ok())
                .and_then(|id| categories.get(&id));
            parser::map_stalker_to_playlist_item(raw, category, kind, added_at)
        })
        .collect()
}

fn catalog_page_signature<'a>(ids: impl Iterator<Item = Option<&'a str>>) -> u64 {
    ids.fold(14_695_981_039_346_656_037_u64, |hash, id| {
        id.unwrap_or_default().bytes().chain([0]).fold(hash, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        })
    })
}

fn ensure_page_advanced(
    next_page: Option<u32>,
    previous_signature: Option<u64>,
    signature: u64,
    page: u32,
    portal_type: &'static str,
) -> Result<(), TuliproxError> {
    if next_page.is_some() && previous_signature == Some(signature) {
        return Err(provider_error(&StalkerError::CatalogIncomplete {
            portal_type,
            reason: format!("page {page} repeated"),
        }));
    }
    Ok(())
}

fn provider_error(err: &StalkerError) -> TuliproxError {
    TuliproxError::ProviderConnection(format!("Stalker client error: {err}"))
}

async fn yield_after_error(
    storage_path: &Path,
    mut checkpoint: StalkerCheckpoint,
    error: TuliproxError,
) -> Result<StalkerRefreshOutcome, TuliproxError> {
    checkpoint.retry_count = checkpoint.retry_count.saturating_add(1);
    if checkpoint.retry_count >= MAX_RETRIES {
        checkpoint.phase = StalkerRefreshPhase::Terminal;
        save_checkpoint(storage_path, &checkpoint).await?;
        return Ok(StalkerRefreshOutcome::Terminal(error));
    }
    save_checkpoint(storage_path, &checkpoint).await?;
    Ok(StalkerRefreshOutcome::Yielded {
        phase: checkpoint.phase,
        processed: checkpoint.processed,
        skipped: checkpoint.skipped_count,
        error: Some(error),
    })
}

#[allow(clippy::too_many_lines)]
pub async fn advance_stalker_refresh(
    app_config: &Arc<AppConfig>,
    api_client: &StalkerApiClient,
    handshake: &StalkerHandshake,
    selection: StalkerClusterSelection,
    storage_path: &Path,
    identity_fingerprint: u64,
    mut budget: StalkerRefreshBudget,
) -> Result<StalkerRefreshOutcome, TuliproxError> {
    let mut checkpoint = match load_checkpoint(storage_path, identity_fingerprint).await? {
        Some(state) if state.selection_mask == selection.mask() => {
            if state.phase == StalkerRefreshPhase::Terminal {
                return Ok(StalkerRefreshOutcome::Terminal(TuliproxError::ProviderConnection(
                    "Stalker refresh reached a terminal state".to_string(),
                )));
            }
            if state.phase == StalkerRefreshPhase::Complete {
                let mut state = StalkerCheckpoint::new(
                    identity_fingerprint,
                    generation_id(),
                    selection.mask(),
                    chrono::Utc::now().timestamp(),
                );
                state.phase = first_phase(selection);
                save_checkpoint(storage_path, &state).await?;
                state
            } else {
                state
            }
        }
        _ => {
            let mut state = StalkerCheckpoint::new(
                identity_fingerprint,
                generation_id(),
                selection.mask(),
                chrono::Utc::now().timestamp(),
            );
            state.phase = first_phase(selection);
            save_checkpoint(storage_path, &state).await?;
            state
        }
    };
    let added_at = checkpoint.started_at;
    let mut live_categories = None;
    let mut vod_categories = None;
    let mut series_categories = None;
    let mut used_episode_ids = None;

    loop {
        if budget.should_yield() {
            return Ok(StalkerRefreshOutcome::Yielded {
                phase: checkpoint.phase,
                processed: checkpoint.processed,
                skipped: checkpoint.skipped_count,
                error: None,
            });
        }
        match checkpoint.phase.clone() {
            StalkerRefreshPhase::LiveBulk => {
                let categories = if let Some(categories) = &live_categories {
                    categories
                } else {
                    match category_map_result(api_client.get_live_categories(handshake).await) {
                        Ok(categories) => live_categories.insert(categories),
                        Err(err) => return yield_after_error(storage_path, checkpoint, err).await,
                    }
                };
                match api_client.get_all_channels(handshake).await {
                    Ok(raw) if raw.is_empty() => {
                        checkpoint.phase = StalkerRefreshPhase::Live { page: 1 };
                        checkpoint.retry_count = 0;
                    }
                    Ok(raw) => {
                        let items = map_items(&raw, categories, StalkerStreamKind::Live, added_at);
                        let path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::Live);
                        snapshot_stalker_items_at(app_config, path.clone(), &items).await?;
                        publish_cluster(
                            storage_path,
                            identity_fingerprint,
                            PublishedCluster::Live(ClusterFiles { generation: checkpoint.generation, data: path }),
                        )
                        .await?;
                        checkpoint.processed = checkpoint.processed.saturating_add(items.len() as u64);
                        checkpoint.phase = next_phase_after_live(selection);
                        checkpoint.retry_count = 0;
                    }
                    Err(err) if err.is_unsupported_catalog_action() => {
                        checkpoint.phase = StalkerRefreshPhase::Live { page: 1 };
                        checkpoint.retry_count = 0;
                    }
                    Err(err) => return yield_after_error(storage_path, checkpoint, provider_error(&err)).await,
                }
            }
            StalkerRefreshPhase::Live { page } => {
                let categories = if let Some(categories) = &live_categories {
                    categories
                } else {
                    match category_map_result(api_client.get_live_categories(handshake).await) {
                        Ok(categories) => live_categories.insert(categories),
                        Err(err) => return yield_after_error(storage_path, checkpoint, err).await,
                    }
                };
                let response = match api_client.get_live_streams_page(handshake, page).await {
                    Ok(response) => response,
                    Err(err) => return yield_after_error(storage_path, checkpoint, provider_error(&err)).await,
                };
                let signature = catalog_page_signature(response.items.iter().map(|item| item.id.as_deref()));
                if let Err(err) = ensure_page_advanced(
                    response.next_page,
                    checkpoint.page_signature,
                    signature,
                    page,
                    "itv",
                ) {
                    return yield_after_error(storage_path, checkpoint, err).await;
                }
                let items = map_items(&response.items, categories, StalkerStreamKind::Live, added_at);
                let path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::Live);
                upsert_stalker_items_at(app_config, &path, &items).await?;
                checkpoint.processed = checkpoint.processed.saturating_add(items.len() as u64);
                checkpoint.phase = if let Some(next_page) = response.next_page {
                    checkpoint.page_signature = Some(signature);
                    StalkerRefreshPhase::Live { page: next_page }
                } else {
                    publish_cluster(
                        storage_path,
                        identity_fingerprint,
                        PublishedCluster::Live(ClusterFiles { generation: checkpoint.generation, data: path }),
                    )
                    .await?;
                    checkpoint.page_signature = None;
                    next_phase_after_live(selection)
                };
                checkpoint.retry_count = 0;
            }
            StalkerRefreshPhase::Vod { page } => {
                let categories = if let Some(categories) = &vod_categories {
                    categories
                } else {
                    match category_map_result(api_client.get_vod_categories(handshake).await) {
                        Ok(categories) => vod_categories.insert(categories),
                        Err(err) => return yield_after_error(storage_path, checkpoint, err).await,
                    }
                };
                let response = match api_client.get_vod_streams_page(handshake, page).await {
                    Ok(response) => response,
                    Err(err) => return yield_after_error(storage_path, checkpoint, provider_error(&err)).await,
                };
                let signature = catalog_page_signature(response.items.iter().map(|item| item.id.as_deref()));
                if let Err(err) = ensure_page_advanced(
                    response.next_page,
                    checkpoint.page_signature,
                    signature,
                    page,
                    "vod",
                ) {
                    return yield_after_error(storage_path, checkpoint, err).await;
                }
                let items = map_items(&response.items, categories, StalkerStreamKind::Movie, added_at);
                let path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::Vod);
                upsert_stalker_items_at(app_config, &path, &items).await?;
                checkpoint.processed = checkpoint.processed.saturating_add(items.len() as u64);
                checkpoint.phase = if let Some(next_page) = response.next_page {
                    checkpoint.page_signature = Some(signature);
                    StalkerRefreshPhase::Vod { page: next_page }
                } else {
                    publish_cluster(
                        storage_path,
                        identity_fingerprint,
                        PublishedCluster::Vod(ClusterFiles { generation: checkpoint.generation, data: path }),
                    )
                    .await?;
                    checkpoint.page_signature = None;
                    next_phase_after_vod(selection)
                };
                checkpoint.retry_count = 0;
            }
            StalkerRefreshPhase::SeriesRoots { page } => {
                let categories = if let Some(categories) = &series_categories {
                    categories
                } else {
                    match category_map_result(api_client.get_series_categories(handshake).await) {
                        Ok(categories) => series_categories.insert(categories),
                        Err(err) => return yield_after_error(storage_path, checkpoint, err).await,
                    }
                };
                let response = match api_client.get_series_list_page(handshake, page).await {
                    Ok(response) => response,
                    Err(err) => return yield_after_error(storage_path, checkpoint, provider_error(&err)).await,
                };
                let signature = catalog_page_signature(response.items.iter().map(|item| item.id.as_deref()));
                if let Err(err) = ensure_page_advanced(
                    response.next_page,
                    checkpoint.page_signature,
                    signature,
                    page,
                    "series",
                ) {
                    return yield_after_error(storage_path, checkpoint, err).await;
                }
                let roots: Vec<_> = response
                    .items
                    .iter()
                    .map(|raw| {
                        let category = raw
                            .category_id
                            .as_deref()
                            .and_then(|value| value.parse::<u32>().ok())
                            .and_then(|id| categories.get(&id));
                        let mut root = parser::map_stalker_series_root(raw, category, added_at);
                        let capabilities = &handshake.profile.portal_capabilities;
                        root.nginx_secure_link = capabilities.nginx_secure_link;
                        root.flussonic_tmp_link = capabilities.flussonic_temporary_link;
                        root.wowza_tmp_link = capabilities.wowza_temporary_link;
                        root.use_http_tmp_link = capabilities.use_http_temporary_link;
                        root
                    })
                    .collect();
                let roots_path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::SeriesRoots);
                upsert_stalker_items_at(app_config, &roots_path, &roots).await?;
                checkpoint.processed = checkpoint.processed.saturating_add(roots.len() as u64);
                checkpoint.phase = match response.next_page {
                    None => {
                        checkpoint.page_signature = None;
                        StalkerRefreshPhase::SeriesDetails { provider_id: None }
                    }
                    Some(next_page) => {
                        checkpoint.page_signature = Some(signature);
                        StalkerRefreshPhase::SeriesRoots { page: next_page }
                    }
                };
                checkpoint.retry_count = 0;
            }
            StalkerRefreshPhase::SeriesDetails { provider_id } => {
                let roots_path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::SeriesRoots);
                let mut roots = load_stalker_items_after(app_config, &roots_path, provider_id, 1).await?;
                let Some(root) = roots.pop() else {
                    let episodes = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::SeriesEpisodes);
                    publish_cluster(
                        storage_path,
                        identity_fingerprint,
                        PublishedCluster::Series(SeriesFiles {
                            generation: checkpoint.generation,
                            roots: roots_path,
                            episodes,
                        }),
                    )
                    .await?;
                    checkpoint.phase = next_phase_after_series(selection);
                    checkpoint.retry_count = 0;
                    save_checkpoint(storage_path, &checkpoint).await?;
                    budget.completed_unit();
                    continue;
                };
                let series_id = root.series_id.unwrap_or(root.stream_id);
                match api_client.get_series_details(handshake, series_id).await {
                    Ok(details) => {
                        let path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::SeriesEpisodes);
                        let used = if let Some(used) = &mut used_episode_ids {
                            used
                        } else {
                            used_episode_ids.insert(
                                prepare_stalker_episode_series_at(app_config, &path, series_id)
                                    .await?,
                            )
                        };
                        let episodes = parser::map_stalker_series_details(&details, &root, added_at, used);
                        upsert_stalker_items_at(app_config, &path, &episodes).await?;
                        checkpoint.processed = checkpoint.processed.saturating_add(episodes.len() as u64);
                        checkpoint.phase = StalkerRefreshPhase::SeriesDetails { provider_id: Some(root.stream_id) };
                        checkpoint.retry_count = 0;
                    }
                    Err(err) => {
                        checkpoint.retry_count = checkpoint.retry_count.saturating_add(1);
                        if checkpoint.retry_count < MAX_RETRIES {
                            save_checkpoint(storage_path, &checkpoint).await?;
                            return Ok(StalkerRefreshOutcome::Yielded {
                                phase: checkpoint.phase,
                                processed: checkpoint.processed,
                                skipped: checkpoint.skipped_count,
                                error: Some(provider_error(&err)),
                            });
                        }
                        checkpoint.skipped_count = checkpoint.skipped_count.saturating_add(1);
                        if checkpoint.skipped_sample.len() < SKIPPED_SAMPLE_LIMIT {
                            checkpoint.skipped_sample.push(series_id);
                        }
                        checkpoint.phase = StalkerRefreshPhase::SeriesDetails { provider_id: Some(root.stream_id) };
                        checkpoint.retry_count = 0;
                    }
                }
            }
            StalkerRefreshPhase::Epg => {
                let path = generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::Epg);
                let attempt_path = path.with_extension("attempt.db");
                snapshot_stalker_epg_at(app_config, &attempt_path, &[]).await?;
                let app_config = Arc::clone(app_config);
                let sink_app_config = Arc::clone(&app_config);
                let batch_path = attempt_path.clone();
                let epg_result = api_client
                    .stream_bulk_epg(handshake, 24, 512, move |batch| {
                        let app_config = Arc::clone(&sink_app_config);
                        let batch_path = batch_path.clone();
                        async move {
                            upsert_stalker_epg_at(&app_config, &batch_path, &batch)
                                .await
                                .map(|_| ())
                                .map_err(|err| StalkerError::Io(std::io::Error::other(err.to_string())))
                        }
                    })
                    .await;
                if let Err(err) = epg_result {
                    remove_stalker_file(&app_config, &attempt_path).await?;
                    return yield_after_error(storage_path, checkpoint, provider_error(&err)).await;
                }
                promote_stalker_file(&app_config, &attempt_path, &path).await?;
                publish_cluster(
                    storage_path,
                    identity_fingerprint,
                    PublishedCluster::Epg(ClusterFiles { generation: checkpoint.generation, data: path }),
                )
                .await?;
                checkpoint.phase = StalkerRefreshPhase::Complete;
                checkpoint.retry_count = 0;
            }
            StalkerRefreshPhase::Complete => {
                clear_checkpoint(storage_path).await?;
                let manifest = load_active_manifest(storage_path, identity_fingerprint).await?;
                cleanup_obsolete_generations(storage_path, &manifest).await?;
                return Ok(StalkerRefreshOutcome::Complete);
            }
            StalkerRefreshPhase::Terminal => {
                clear_checkpoint(storage_path).await?;
                return Ok(StalkerRefreshOutcome::Terminal(TuliproxError::ProviderConnection(
                    "Stalker refresh reached a terminal state".to_string(),
                )));
            }
        }
        save_checkpoint(storage_path, &checkpoint).await?;
        budget.completed_unit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_budget_yields_after_completed_unit() {
        let mut budget = StalkerRefreshBudget::units(1);
        assert!(!budget.should_yield());
        budget.completed_unit();
        assert!(budget.should_yield());
    }

    #[test]
    fn phase_progression_skips_disabled_clusters() {
        let selection = StalkerClusterSelection { live: false, vod: true, series: false, epg: false };
        assert_eq!(next_phase_after_live(selection), StalkerRefreshPhase::Vod { page: 1 });
        assert_eq!(next_phase_after_vod(selection), StalkerRefreshPhase::Complete);
    }

    #[test]
    fn requested_clusters_limit_the_refresh_selection() {
        let input = ConfigInput::default();
        let selection = StalkerClusterSelection::requested(&input, &[StalkerCluster::Series]);
        assert!(!selection.live);
        assert!(!selection.vod);
        assert!(selection.series);
        assert!(!selection.epg);
    }

    #[test]
    fn category_errors_are_not_silently_downgraded() {
        let result = category_map_result(Err(StalkerError::BodyDecode { message: "broken".to_string() }));
        assert!(result.is_err());
    }

    #[test]
    fn repeated_page_is_rejected_while_more_pages_are_advertised() {
        assert!(ensure_page_advanced(Some(3), Some(17), 17, 2, "vod").is_err());
        assert!(ensure_page_advanced(None, Some(17), 17, 2, "vod").is_ok());
    }
}
