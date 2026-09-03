use super::stalker::StalkerCluster;
use shared::{
    error::TuliproxError,
    model::{stalker::StalkerStreamKind, stalker_item::StalkerPlaylistItem, XtreamCluster},
};
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tuliprox_core::model::{
    evaluate_update_quality, AppConfig, ClusterUpdateRejection, ConfigInput, ConfigInputFlags,
    ConfigInputUpdateQuality, UpdateQualityDecision,
};
use tuliprox_iptv::stalker::{
    catalog::{StalkerCategory, StalkerRawItem},
    client::StalkerApiClient,
    error::StalkerError,
    parser,
    profile::StalkerHandshake,
};
use tuliprox_repository::{
    stalker_generation_repository::{
        cleanup_obsolete_generations, clear_checkpoint, generation_data_path, load_active_manifest, load_checkpoint,
        publish_selection, save_checkpoint, StalkerCheckpoint, StalkerGenerationData, StalkerRefreshPhase,
    },
    stalker_repository::{
        count_stalker_items_at, load_stalker_items_after, prepare_stalker_episode_series_at, promote_stalker_file,
        remove_stalker_file, snapshot_stalker_epg_at, snapshot_stalker_items_at, upsert_stalker_epg_at,
        upsert_stalker_items_at,
    },
};

const MAX_RETRIES: u8 = 3;
const SKIPPED_SAMPLE_LIMIT: usize = 32;
const LIVE_SELECTION: u8 = 0b0001;
const VOD_SELECTION: u8 = 0b0010;
const SERIES_SELECTION: u8 = 0b0100;
const EPG_SELECTION: u8 = 0b1000;

pub enum StalkerRefreshLimit {
    Deadline(Instant),
    Units { remaining: usize },
    Unlimited,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StalkerRefreshMode {
    ServerSlice,
    Parallel,
    Complete,
}

impl StalkerRefreshMode {
    pub fn budget(self) -> StalkerRefreshBudget {
        match self {
            Self::ServerSlice => StalkerRefreshBudget::deadline(Instant::now() + std::time::Duration::from_mins(45)),
            Self::Parallel => StalkerRefreshBudget::units(8),
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

    fn units(remaining: usize) -> Self { Self { limit: StalkerRefreshLimit::Units { remaining } } }

    fn completed_unit(&mut self) {
        if let StalkerRefreshLimit::Units { remaining } = &mut self.limit {
            *remaining = remaining.saturating_sub(1);
        }
    }

    fn should_yield(&self) -> bool {
        match self.limit {
            StalkerRefreshLimit::Deadline(deadline) => Instant::now() >= deadline,
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
        u8::from(self.live) | (u8::from(self.vod) << 1) | (u8::from(self.series) << 2) | (u8::from(self.epg) << 3)
    }
}

/// Requested clusters and the publication policy applied once their generation is complete.
#[derive(Clone, Copy)]
pub struct StalkerRefreshPlan {
    selection: StalkerClusterSelection,
    update_quality: ConfigInputUpdateQuality,
}

impl StalkerRefreshPlan {
    pub fn requested(input: &ConfigInput, requested: &[StalkerCluster]) -> Self {
        let selection = StalkerClusterSelection::requested(input, requested);
        let update_quality =
            input.options.as_ref().map_or_else(ConfigInputUpdateQuality::default, |options| options.update_quality);
        Self { selection, update_quality }
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
    if selection.epg {
        StalkerRefreshPhase::Epg
    } else {
        StalkerRefreshPhase::Complete
    }
}

#[derive(Debug)]
pub enum StalkerRefreshOutcome {
    Complete { quality_rejections: Vec<ClusterUpdateRejection> },
    Yielded { phase: StalkerRefreshPhase, processed: u64, skipped: u64, error: Option<TuliproxError> },
    Terminal(TuliproxError),
}

fn generation_id() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX))
}

async fn load_or_start_checkpoint(
    storage_path: &Path,
    identity_fingerprint: u64,
    selection: StalkerClusterSelection,
) -> Result<StalkerCheckpoint, TuliproxError> {
    if let Some(state) = load_checkpoint(storage_path, identity_fingerprint).await? {
        if state.selection_mask == selection.mask() {
            return Ok(state);
        }
    }

    let mut state =
        StalkerCheckpoint::new(identity_fingerprint, generation_id(), selection.mask(), chrono::Utc::now().timestamp());
    state.phase = first_phase(selection);
    save_checkpoint(storage_path, &state).await?;
    Ok(state)
}

async fn evaluate_completed_refresh_publication(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    identity_fingerprint: u64,
    checkpoint: &StalkerCheckpoint,
    update_quality: ConfigInputUpdateQuality,
) -> Result<StalkerGenerationPublication, TuliproxError> {
    let needs_quality_evaluation = selection_needs_quality_evaluation(checkpoint.selection_mask, update_quality);
    if needs_quality_evaluation {
        let active_manifest = load_active_manifest(storage_path, identity_fingerprint).await?;
        evaluate_generation_publication(
            app_config,
            storage_path,
            checkpoint.generation,
            checkpoint.selection_mask,
            update_quality,
            &active_manifest,
        )
        .await
    } else {
        Ok(StalkerGenerationPublication {
            accepted_selection_mask: checkpoint.selection_mask,
            quality_rejections: Vec::new(),
        })
    }
}

async fn ensure_completed_refresh_has_usable_playlist(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    identity_fingerprint: u64,
    checkpoint: &StalkerCheckpoint,
    accepted_selection_mask: u8,
) -> Result<(), TuliproxError> {
    let accepted_media_selection = accepted_selection_mask & (LIVE_SELECTION | VOD_SELECTION | SERIES_SELECTION);
    if checkpoint.processed == 0 && accepted_media_selection != 0 {
        let active = load_active_manifest(storage_path, identity_fingerprint).await?;
        let mut retained_has_items = false;
        if accepted_selection_mask & LIVE_SELECTION == 0 {
            if let Some(files) = active.live.as_ref() {
                retained_has_items = !load_stalker_items_after(app_config, &files.data, None, 1).await?.is_empty();
            }
        }
        if !retained_has_items && accepted_selection_mask & VOD_SELECTION == 0 {
            if let Some(files) = active.vod.as_ref() {
                retained_has_items = !load_stalker_items_after(app_config, &files.data, None, 1).await?.is_empty();
            }
        }
        if !retained_has_items && accepted_selection_mask & SERIES_SELECTION == 0 {
            if let Some(files) = active.series.as_ref() {
                retained_has_items = !load_stalker_items_after(app_config, &files.roots, None, 1).await?.is_empty()
                    || !load_stalker_items_after(app_config, &files.episodes, None, 1).await?.is_empty();
            }
        }
        if !retained_has_items {
            clear_checkpoint(storage_path).await?;
            cleanup_obsolete_generations(storage_path, &active).await?;
            return Err(TuliproxError::RepositoryPlaylist(
                "Refusing to publish an empty Stalker playlist; existing data was retained".to_string(),
            ));
        }
    }
    Ok(())
}

async fn publish_completed_refresh(
    storage_path: &Path,
    identity_fingerprint: u64,
    checkpoint: &StalkerCheckpoint,
    publication: StalkerGenerationPublication,
) -> Result<Vec<ClusterUpdateRejection>, TuliproxError> {
    let manifest = publish_selection(
        storage_path,
        identity_fingerprint,
        checkpoint.generation,
        publication.accepted_selection_mask,
    )
    .await?;
    clear_checkpoint(storage_path).await?;
    cleanup_obsolete_generations(storage_path, &manifest).await?;
    Ok(publication.quality_rejections)
}

#[cfg(test)]
async fn finish_completed_refresh(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    identity_fingerprint: u64,
    checkpoint: &StalkerCheckpoint,
    update_quality: ConfigInputUpdateQuality,
) -> Result<Vec<ClusterUpdateRejection>, TuliproxError> {
    let publication = evaluate_completed_refresh_publication(
        app_config,
        storage_path,
        identity_fingerprint,
        checkpoint,
        update_quality,
    )
    .await?;
    publish_completed_refresh(storage_path, identity_fingerprint, checkpoint, publication).await
}

async fn finish_completed_refresh_checked(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    identity_fingerprint: u64,
    checkpoint: &StalkerCheckpoint,
    update_quality: ConfigInputUpdateQuality,
) -> Result<Vec<ClusterUpdateRejection>, TuliproxError> {
    let publication = evaluate_completed_refresh_publication(
        app_config,
        storage_path,
        identity_fingerprint,
        checkpoint,
        update_quality,
    )
    .await?;
    ensure_completed_refresh_has_usable_playlist(
        app_config,
        storage_path,
        identity_fingerprint,
        checkpoint,
        publication.accepted_selection_mask,
    )
    .await?;
    publish_completed_refresh(storage_path, identity_fingerprint, checkpoint, publication).await
}

struct StalkerGenerationPublication {
    accepted_selection_mask: u8,
    quality_rejections: Vec<ClusterUpdateRejection>,
}

fn selection_needs_quality_evaluation(requested_selection_mask: u8, update_quality: ConfigInputUpdateQuality) -> bool {
    [StalkerCluster::Live, StalkerCluster::Vod, StalkerCluster::Series].into_iter().any(|cluster| {
        requested_selection_mask & selection_bit(cluster) != 0
            && update_quality.threshold(quality_cluster(cluster)) != 0
    })
}

const fn selection_bit(cluster: StalkerCluster) -> u8 {
    match cluster {
        StalkerCluster::Live => LIVE_SELECTION,
        StalkerCluster::Vod => VOD_SELECTION,
        StalkerCluster::Series => SERIES_SELECTION,
    }
}

const fn quality_cluster(cluster: StalkerCluster) -> XtreamCluster {
    match cluster {
        StalkerCluster::Live => XtreamCluster::Live,
        StalkerCluster::Vod => XtreamCluster::Video,
        StalkerCluster::Series => XtreamCluster::Series,
    }
}

async fn count_stalker_cluster_files(
    app_config: &Arc<AppConfig>,
    primary: &Path,
    secondary: Option<&Path>,
) -> Result<usize, TuliproxError> {
    let primary_count = count_stalker_items_at(app_config, primary).await?.unwrap_or_default();
    let secondary_count = match secondary {
        Some(path) => count_stalker_items_at(app_config, path).await?.unwrap_or_default(),
        None => 0,
    };
    primary_count.checked_add(secondary_count).ok_or_else(|| {
        TuliproxError::RepositoryStalker("Stalker cluster item count exceeds platform capacity".to_string())
    })
}

async fn count_candidate_cluster(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    generation: u64,
    cluster: StalkerCluster,
) -> Result<usize, TuliproxError> {
    let primary_data = match cluster {
        StalkerCluster::Live => StalkerGenerationData::Live,
        StalkerCluster::Vod => StalkerGenerationData::Vod,
        StalkerCluster::Series => StalkerGenerationData::SeriesRoots,
    };
    let primary = generation_data_path(storage_path, generation, primary_data);
    let secondary = (cluster == StalkerCluster::Series)
        .then(|| generation_data_path(storage_path, generation, StalkerGenerationData::SeriesEpisodes));
    count_stalker_cluster_files(app_config, &primary, secondary.as_deref()).await
}

async fn count_active_cluster(
    app_config: &Arc<AppConfig>,
    manifest: &tuliprox_repository::stalker_generation_repository::StalkerActiveManifest,
    cluster: StalkerCluster,
) -> Result<Option<usize>, TuliproxError> {
    match cluster {
        StalkerCluster::Live => match manifest.live.as_ref() {
            Some(files) => count_stalker_cluster_files(app_config, &files.data, None).await.map(Some),
            None => Ok(None),
        },
        StalkerCluster::Vod => match manifest.vod.as_ref() {
            Some(files) => count_stalker_cluster_files(app_config, &files.data, None).await.map(Some),
            None => Ok(None),
        },
        StalkerCluster::Series => match manifest.series.as_ref() {
            Some(files) => count_stalker_cluster_files(app_config, &files.roots, Some(&files.episodes)).await.map(Some),
            None => Ok(None),
        },
    }
}

fn rejection_from_decision(
    cluster: XtreamCluster,
    current_count: Option<usize>,
    candidate_count: usize,
    threshold: u8,
    decision: UpdateQualityDecision,
) -> Option<ClusterUpdateRejection> {
    match decision {
        UpdateQualityDecision::Rejected { current, candidate, threshold, quality } => Some(ClusterUpdateRejection {
            cluster,
            current_count: current,
            candidate_count: candidate,
            threshold,
            quality,
        }),
        UpdateQualityDecision::RejectedWithoutBaseline => Some(ClusterUpdateRejection {
            cluster,
            current_count: current_count.unwrap_or_default(),
            candidate_count,
            threshold: threshold.min(100),
            quality: 0,
        }),
        UpdateQualityDecision::Disabled
        | UpdateQualityDecision::BootstrapAccepted { .. }
        | UpdateQualityDecision::Accepted { .. } => None,
    }
}

async fn evaluate_generation_publication(
    app_config: &Arc<AppConfig>,
    storage_path: &Path,
    generation: u64,
    requested_selection_mask: u8,
    update_quality: ConfigInputUpdateQuality,
    active_manifest: &tuliprox_repository::stalker_generation_repository::StalkerActiveManifest,
) -> Result<StalkerGenerationPublication, TuliproxError> {
    let mut accepted_selection_mask = requested_selection_mask;
    let mut quality_rejections = Vec::new();
    for cluster in [StalkerCluster::Live, StalkerCluster::Vod, StalkerCluster::Series] {
        let selection_bit = selection_bit(cluster);
        if requested_selection_mask & selection_bit == 0 {
            continue;
        }
        let report_cluster = quality_cluster(cluster);
        let threshold = update_quality.threshold(report_cluster);
        if threshold == 0 {
            continue;
        }
        let candidate_count = count_candidate_cluster(app_config, storage_path, generation, cluster).await?;
        let current_count = count_active_cluster(app_config, active_manifest, cluster).await?;
        let decision = evaluate_update_quality(current_count, candidate_count, threshold);
        if let Some(rejection) =
            rejection_from_decision(report_cluster, current_count, candidate_count, threshold, decision)
        {
            accepted_selection_mask &= !selection_bit;
            if cluster == StalkerCluster::Live {
                accepted_selection_mask &= !EPG_SELECTION;
            }
            quality_rejections.push(rejection);
        }
    }
    Ok(StalkerGenerationPublication { accepted_selection_mask, quality_rejections })
}

fn category_map(categories: Vec<StalkerCategory>) -> HashMap<u32, StalkerCategory> {
    categories.into_iter().filter_map(|category| category.id.parse().ok().map(|id| (id, category))).collect()
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
            let category =
                raw.category_id().and_then(|value| value.parse::<u32>().ok()).and_then(|id| categories.get(&id));
            parser::map_stalker_to_playlist_item(raw, category, kind, added_at)
        })
        .collect()
}

fn catalog_page_signature<'a>(ids: impl Iterator<Item = Option<&'a str>>) -> u64 {
    ids.fold(14_695_981_039_346_656_037_u64, |hash, id| {
        id.unwrap_or_default()
            .bytes()
            .chain([0])
            .fold(hash, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211))
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

async fn finish_terminal_refresh(storage_path: &Path) -> Result<StalkerRefreshOutcome, TuliproxError> {
    clear_checkpoint(storage_path).await?;
    Ok(StalkerRefreshOutcome::Terminal(TuliproxError::ProviderConnection(
        "Stalker refresh reached a terminal state".to_string(),
    )))
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
    refresh_plan: StalkerRefreshPlan,
    storage_path: &Path,
    identity_fingerprint: u64,
    mut budget: StalkerRefreshBudget,
) -> Result<StalkerRefreshOutcome, TuliproxError> {
    let StalkerRefreshPlan { selection, update_quality } = refresh_plan;
    let mut checkpoint = load_or_start_checkpoint(storage_path, identity_fingerprint, selection).await?;
    if checkpoint.phase == StalkerRefreshPhase::Terminal {
        return finish_terminal_refresh(storage_path).await;
    }
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
                        let path =
                            generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::Live);
                        snapshot_stalker_items_at(app_config, path.clone(), &items).await?;
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
                if let Err(err) =
                    ensure_page_advanced(response.next_page, checkpoint.page_signature, signature, page, "itv")
                {
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
                if let Err(err) =
                    ensure_page_advanced(response.next_page, checkpoint.page_signature, signature, page, "vod")
                {
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
                if let Err(err) =
                    ensure_page_advanced(response.next_page, checkpoint.page_signature, signature, page, "series")
                {
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
                let roots_path =
                    generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::SeriesRoots);
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
                let roots_path =
                    generation_data_path(storage_path, checkpoint.generation, StalkerGenerationData::SeriesRoots);
                let mut roots = load_stalker_items_after(app_config, &roots_path, provider_id, 1).await?;
                let Some(root) = roots.pop() else {
                    checkpoint.phase = next_phase_after_series(selection);
                    checkpoint.retry_count = 0;
                    save_checkpoint(storage_path, &checkpoint).await?;
                    budget.completed_unit();
                    continue;
                };
                let series_id = root.series_id.unwrap_or(root.stream_id);
                match api_client.get_series_details(handshake, series_id).await {
                    Ok(details) => {
                        let path = generation_data_path(
                            storage_path,
                            checkpoint.generation,
                            StalkerGenerationData::SeriesEpisodes,
                        );
                        let used = if let Some(used) = &mut used_episode_ids {
                            used
                        } else {
                            used_episode_ids
                                .insert(prepare_stalker_episode_series_at(app_config, &path, series_id).await?)
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
                checkpoint.phase = StalkerRefreshPhase::Complete;
                checkpoint.retry_count = 0;
            }
            StalkerRefreshPhase::Complete => {
                let quality_rejections = finish_completed_refresh_checked(
                    app_config,
                    storage_path,
                    identity_fingerprint,
                    &checkpoint,
                    update_quality,
                )
                .await?;
                return Ok(StalkerRefreshOutcome::Complete { quality_rejections });
            }
            StalkerRefreshPhase::Terminal => {
                return finish_terminal_refresh(storage_path).await;
            }
        }
        save_checkpoint(storage_path, &checkpoint).await?;
        budget.completed_unit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::{ArcSwap, ArcSwapOption};
    use shared::{
        model::{ConfigInputUpdateQualityDto, ConfigPaths},
        utils::Internable,
    };
    use tuliprox_core::{
        model::{ApiProxyConfig, Config, CustomStreamResponse, HdHomeRunConfig, MediaToolCapabilities, SourcesConfig},
        utils::FileLockManager,
    };
    use tuliprox_repository::stalker_generation_repository::{
        load_active_manifest, save_active_manifest, ClusterFiles, SeriesFiles, StalkerActiveManifest,
    };

    fn test_app_config(storage_dir: &Path) -> Arc<AppConfig> {
        Arc::new(AppConfig {
            config: Arc::new(ArcSwap::from_pointee(Config {
                storage_dir: storage_dir.to_string_lossy().into_owned(),
                ..Config::default()
            })),
            sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
            hdhomerun: Arc::new(ArcSwapOption::<HdHomeRunConfig>::default()),
            api_proxy: Arc::new(ArcSwapOption::<ApiProxyConfig>::default()),
            file_locks: Arc::new(FileLockManager::default()),
            paths: Arc::new(ArcSwap::from_pointee(ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: storage_dir.to_string_lossy().into_owned(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            })),
            custom_stream_response: Arc::new(ArcSwapOption::<CustomStreamResponse>::default()),
            access_token_secret: [0; 32],
            encrypt_secret: [0; 16],
            media_tools: Arc::new(MediaToolCapabilities::new()),
        })
    }

    fn update_quality(live: u8, vod: u8, series: u8) -> ConfigInputUpdateQuality {
        ConfigInputUpdateQuality::from(&ConfigInputUpdateQualityDto { live, vod, series })
    }

    fn stalker_items(count: usize, kind: StalkerStreamKind, series_roots: bool) -> Vec<StalkerPlaylistItem> {
        (0..count)
            .map(|index| StalkerPlaylistItem {
                stream_id: u32::try_from(index + 1).unwrap_or(u32::MAX),
                name: format!("item-{index}").intern(),
                stream_kind: kind,
                is_series: series_roots,
                ..StalkerPlaylistItem::default()
            })
            .collect()
    }

    async fn write_generation_cluster(
        app_config: &Arc<AppConfig>,
        storage_path: &Path,
        generation: u64,
        cluster: StalkerCluster,
        primary_count: usize,
        episode_count: usize,
    ) -> Result<(), TuliproxError> {
        let (primary_data, kind, series_roots) = match cluster {
            StalkerCluster::Live => (StalkerGenerationData::Live, StalkerStreamKind::Live, false),
            StalkerCluster::Vod => (StalkerGenerationData::Vod, StalkerStreamKind::Movie, false),
            StalkerCluster::Series => (StalkerGenerationData::SeriesRoots, StalkerStreamKind::Episode, true),
        };
        let primary_path = generation_data_path(storage_path, generation, primary_data);
        let primary_items = stalker_items(primary_count, kind, series_roots);
        if primary_items.is_empty() {
            upsert_stalker_items_at(app_config, &primary_path, &primary_items).await?;
        } else {
            snapshot_stalker_items_at(app_config, primary_path, &primary_items).await?;
        }
        if cluster == StalkerCluster::Series {
            let episode_path = generation_data_path(storage_path, generation, StalkerGenerationData::SeriesEpisodes);
            snapshot_stalker_items_at(
                app_config,
                episode_path,
                &stalker_items(episode_count, StalkerStreamKind::Episode, false),
            )
            .await?;
        }
        Ok(())
    }

    fn complete_manifest(storage_path: &Path, identity_fingerprint: u64, generation: u64) -> StalkerActiveManifest {
        StalkerActiveManifest {
            schema: StalkerActiveManifest::empty(identity_fingerprint).schema,
            identity_fingerprint,
            live: Some(ClusterFiles {
                generation,
                data: generation_data_path(storage_path, generation, StalkerGenerationData::Live),
            }),
            vod: Some(ClusterFiles {
                generation,
                data: generation_data_path(storage_path, generation, StalkerGenerationData::Vod),
            }),
            series: Some(SeriesFiles {
                generation,
                roots: generation_data_path(storage_path, generation, StalkerGenerationData::SeriesRoots),
                episodes: generation_data_path(storage_path, generation, StalkerGenerationData::SeriesEpisodes),
            }),
            epg: Some(ClusterFiles {
                generation,
                data: generation_data_path(storage_path, generation, StalkerGenerationData::Epg),
            }),
        }
    }

    #[tokio::test]
    async fn terminal_checkpoint_is_cleared_after_restart() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let selection = StalkerClusterSelection { live: true, vod: false, series: false, epg: false };
        let mut checkpoint = StalkerCheckpoint::new(17, 23, selection.mask(), 123);
        checkpoint.phase = StalkerRefreshPhase::Terminal;
        save_checkpoint(temp.path(), &checkpoint).await?;

        let outcome = finish_terminal_refresh(temp.path()).await?;

        assert!(matches!(outcome, StalkerRefreshOutcome::Terminal(_)));
        assert!(load_checkpoint(temp.path(), 17).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn complete_checkpoint_is_resumed_without_new_generation() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let selection = StalkerClusterSelection { live: true, vod: true, series: false, epg: false };
        let mut checkpoint = StalkerCheckpoint::new(17, 23, selection.mask(), 123);
        checkpoint.phase = StalkerRefreshPhase::Complete;
        save_checkpoint(temp.path(), &checkpoint).await?;

        let resumed = load_or_start_checkpoint(temp.path(), 17, selection).await?;

        assert_eq!(resumed.generation, 23);
        assert_eq!(resumed.phase, StalkerRefreshPhase::Complete);
        Ok(())
    }

    #[tokio::test]
    async fn empty_complete_refresh_keeps_the_active_manifest() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let mut active = tuliprox_repository::stalker_generation_repository::StalkerActiveManifest::empty(17);
        active.live = Some(tuliprox_repository::stalker_generation_repository::ClusterFiles {
            generation: 11,
            data: temp.path().join("old-live.db"),
        });
        tuliprox_repository::stalker_generation_repository::save_active_manifest(temp.path(), &active).await?;

        let selection = StalkerClusterSelection { live: true, vod: false, series: false, epg: false };
        let mut checkpoint = StalkerCheckpoint::new(17, 23, selection.mask(), 123);
        checkpoint.phase = StalkerRefreshPhase::Complete;
        save_checkpoint(temp.path(), &checkpoint).await?;

        let result = finish_completed_refresh_checked(
            &app_config,
            temp.path(),
            17,
            &checkpoint,
            ConfigInputUpdateQuality::default(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(load_active_manifest(temp.path(), 17).await?, active);
        assert!(load_checkpoint(temp.path(), 17).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn complete_publication_is_idempotent_after_restart() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let mut checkpoint = StalkerCheckpoint::new(17, 23, 0b0011, 123);
        checkpoint.phase = StalkerRefreshPhase::Complete;
        save_checkpoint(temp.path(), &checkpoint).await?;

        finish_completed_refresh(&app_config, temp.path(), 17, &checkpoint, ConfigInputUpdateQuality::default())
            .await?;
        save_checkpoint(temp.path(), &checkpoint).await?;
        finish_completed_refresh(&app_config, temp.path(), 17, &checkpoint, ConfigInputUpdateQuality::default())
            .await?;

        let manifest = load_active_manifest(temp.path(), 17).await?;
        assert_eq!(manifest.live.as_ref().map(|files| files.generation), Some(23));
        assert_eq!(manifest.vod.as_ref().map(|files| files.generation), Some(23));
        assert!(load_checkpoint(temp.path(), 17).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn mixed_quality_publication_replaces_only_accepted_manifest_entries(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let identity = 17;
        let active_generation = 41;
        let candidate_generation = 42;
        write_generation_cluster(&app_config, temp.path(), active_generation, StalkerCluster::Live, 100, 0).await?;
        write_generation_cluster(&app_config, temp.path(), active_generation, StalkerCluster::Vod, 100, 0).await?;
        write_generation_cluster(&app_config, temp.path(), active_generation, StalkerCluster::Series, 40, 60).await?;
        let active = complete_manifest(temp.path(), identity, active_generation);
        save_active_manifest(temp.path(), &active).await?;
        write_generation_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Live, 90, 0).await?;
        write_generation_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Vod, 89, 0).await?;
        write_generation_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Series, 45, 65)
            .await?;
        let checkpoint = StalkerCheckpoint::new(identity, candidate_generation, 0b1111, 123);

        let rejections =
            finish_completed_refresh(&app_config, temp.path(), identity, &checkpoint, update_quality(90, 90, 90))
                .await?;

        assert_eq!(
            rejections,
            vec![ClusterUpdateRejection {
                cluster: XtreamCluster::Video,
                current_count: 100,
                candidate_count: 89,
                threshold: 90,
                quality: 89,
            }]
        );
        let published = load_active_manifest(temp.path(), identity).await?;
        assert_eq!(published.live.as_ref().map(|files| files.generation), Some(candidate_generation));
        assert_eq!(published.vod, active.vod);
        assert_eq!(published.series.as_ref().map(|files| files.generation), Some(candidate_generation));
        assert_eq!(published.epg.as_ref().map(|files| files.generation), Some(candidate_generation));
        Ok(())
    }

    #[tokio::test]
    async fn rejected_live_generation_keeps_active_live_and_epg() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let identity = 23;
        let active_generation = 7;
        let candidate_generation = 8;
        write_generation_cluster(&app_config, temp.path(), active_generation, StalkerCluster::Live, 100, 0).await?;
        let mut active = StalkerActiveManifest::empty(identity);
        active.live = Some(ClusterFiles {
            generation: active_generation,
            data: generation_data_path(temp.path(), active_generation, StalkerGenerationData::Live),
        });
        active.epg = Some(ClusterFiles {
            generation: active_generation,
            data: generation_data_path(temp.path(), active_generation, StalkerGenerationData::Epg),
        });
        save_active_manifest(temp.path(), &active).await?;
        write_generation_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Live, 89, 0).await?;
        let checkpoint = StalkerCheckpoint::new(identity, candidate_generation, LIVE_SELECTION | EPG_SELECTION, 123);

        let rejections =
            finish_completed_refresh(&app_config, temp.path(), identity, &checkpoint, update_quality(90, 0, 0)).await?;

        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].cluster, XtreamCluster::Live);
        let published = load_active_manifest(temp.path(), identity).await?;
        assert_eq!(published.live, active.live);
        assert_eq!(published.epg, active.epg);
        Ok(())
    }

    #[tokio::test]
    async fn nonempty_bootstrap_generation_is_published() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let identity = 29;
        let generation = 3;
        write_generation_cluster(&app_config, temp.path(), generation, StalkerCluster::Series, 2, 3).await?;
        let checkpoint = StalkerCheckpoint::new(identity, generation, SERIES_SELECTION, 123);

        let rejections =
            finish_completed_refresh(&app_config, temp.path(), identity, &checkpoint, update_quality(0, 0, 100))
                .await?;

        assert!(rejections.is_empty());
        let published = load_active_manifest(temp.path(), identity).await?;
        assert_eq!(published.series.as_ref().map(|files| files.generation), Some(generation));
        Ok(())
    }

    #[tokio::test]
    async fn empty_bootstrap_generation_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let identity = 30;
        let generation = 4;
        write_generation_cluster(&app_config, temp.path(), generation, StalkerCluster::Live, 0, 0).await?;
        let checkpoint = StalkerCheckpoint::new(identity, generation, LIVE_SELECTION | EPG_SELECTION, 123);

        let rejections =
            finish_completed_refresh(&app_config, temp.path(), identity, &checkpoint, update_quality(90, 0, 0)).await?;

        assert_eq!(
            rejections,
            vec![ClusterUpdateRejection {
                cluster: XtreamCluster::Live,
                current_count: 0,
                candidate_count: 0,
                threshold: 90,
                quality: 0,
            }]
        );
        let published = load_active_manifest(temp.path(), identity).await?;
        assert!(published.live.is_none());
        assert!(published.epg.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn rejected_series_counts_roots_and_episodes_without_blocking_accepted_live_generation(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let app_config = test_app_config(temp.path());
        let identity = 31;
        let active_generation = 11;
        let candidate_generation = 12;
        write_generation_cluster(&app_config, temp.path(), active_generation, StalkerCluster::Live, 10, 0).await?;
        write_generation_cluster(&app_config, temp.path(), active_generation, StalkerCluster::Series, 2, 8).await?;
        let mut active = complete_manifest(temp.path(), identity, active_generation);
        active.vod = None;
        save_active_manifest(temp.path(), &active).await?;
        write_generation_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Live, 10, 0).await?;
        write_generation_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Series, 2, 7).await?;
        let checkpoint = StalkerCheckpoint::new(identity, candidate_generation, LIVE_SELECTION | SERIES_SELECTION, 123);

        assert_eq!(count_active_cluster(&app_config, &active, StalkerCluster::Series).await?, Some(10));
        assert_eq!(
            count_candidate_cluster(&app_config, temp.path(), candidate_generation, StalkerCluster::Series).await?,
            9
        );

        let rejections =
            finish_completed_refresh(&app_config, temp.path(), identity, &checkpoint, update_quality(100, 0, 100))
                .await?;

        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].cluster, XtreamCluster::Series);
        assert_eq!(rejections[0].current_count, 10);
        assert_eq!(rejections[0].candidate_count, 9);
        let published = load_active_manifest(temp.path(), identity).await?;
        assert_eq!(published.live.as_ref().map(|files| files.generation), Some(candidate_generation));
        assert_eq!(published.series, active.series);
        Ok(())
    }

    #[test]
    fn unit_budget_yields_after_completed_unit() {
        let mut budget = StalkerRefreshBudget::units(1);
        assert!(!budget.should_yield());
        budget.completed_unit();
        assert!(budget.should_yield());
    }

    #[test]
    fn parallel_mode_uses_a_bounded_work_unit_budget() {
        let mut budget = StalkerRefreshMode::Parallel.budget();
        assert!(!budget.should_yield());
        for _ in 0..64 {
            budget.completed_unit();
        }
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
    fn map_items_uses_tv_genre_id_for_live_category() {
        let raw: StalkerRawItem = serde_json::from_value(serde_json::json!({
            "id": "590",
            "name": "News",
            "tv_genre_id": "10"
        }))
        .expect("raw item");
        let categories = HashMap::from([(
            10,
            StalkerCategory { id: "10".to_string(), title: "News".to_string(), alias: None, number: 1 },
        )]);

        let items = map_items(&[raw], &categories, StalkerStreamKind::Live, 0);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].category_id, 10);
        assert_eq!(items[0].category_name.as_ref(), "News");
    }

    #[test]
    fn repeated_page_is_rejected_while_more_pages_are_advertised() {
        assert!(ensure_page_advanced(Some(3), Some(17), 17, 2, "vod").is_err());
        assert!(ensure_page_advanced(None, Some(17), 17, 2, "vod").is_ok());
    }
}
