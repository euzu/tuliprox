use crate::api::model::UpdateTask;
use crate::api::model::{ActiveProviderManager, ProviderHandle, ProviderIdType, ResolveReason, ResolveReasonSet};
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::{AppConfig, ConfigTarget, MetadataUpdateConfig};
use crate::model::{ConfigInput, ConfigInputFlags, InputSource};
use crate::processing::parser::xtream::create_xtream_series_episode_url;
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::processing::processor::playlist::{PlaylistProcessingContext, ProcessingPipe};
use crate::processing::processor::{
    create_resolve_options_function_for_xtream_target, process_foreground_retry_once, ResolveOptions,
    ResolveOptionsFlags, FOREGROUND_BATCH_SIZE as BATCH_SIZE, FOREGROUND_MIN_RETRY_DELAY_SECS,
    FOREGROUND_RETRY_BATCH_MAX_SIZE as RETRY_BATCH_MAX_SIZE,
};
use crate::ptt::ptt_parse_title;
use crate::repository::persists_input_series_info;
use crate::repository::{
    get_input_storage_path, persist_input_series_info_batch, MemoryPlaylistSource, PlaylistSource,
};
use crate::repository::{xtream_get_file_path, BPlusTreeQuery};
use crate::utils::{debug_if_enabled, xtream};
use log::{debug, error, info, log_enabled, trace, warn, Level};
use parking_lot::Mutex;
use serde_json::Value;
use shared::error::TuliproxError;
use shared::model::{
    MediaQuality, PlaylistEntry, PlaylistItem, SeriesStreamProperties, StreamProperties, XtreamPlaylistItem,
    XtreamSeriesInfo,
};
use shared::model::{PlaylistGroup, PlaylistItemType, XtreamCluster};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

create_resolve_options_function_for_xtream_target!(series);

#[derive(Debug, Clone, Copy)]
pub struct SeriesProbeSettings {
    pub timeout_secs: u64,
    pub analyze_duration_micros: u64,
    pub probe_size_bytes: u64,
}

impl SeriesProbeSettings {
    pub fn from_metadata_update(metadata_update: Option<&MetadataUpdateConfig>) -> Self {
        let defaults = MetadataUpdateConfig::default();
        Self {
            timeout_secs: metadata_update.and_then(|cfg| cfg.ffprobe_timeout).unwrap_or(60),
            analyze_duration_micros: metadata_update
                .map_or(defaults.ffprobe_analyze_duration_micros, |cfg| cfg.ffprobe_analyze_duration_micros),
            probe_size_bytes: metadata_update
                .map_or(defaults.ffprobe_probe_size_bytes, |cfg| cfg.ffprobe_probe_size_bytes),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn playlist_resolve_series(
    ctx: &PlaylistProcessingContext,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    pipe: &ProcessingPipe,
    provider_fpl: &mut FetchedPlaylist<'_>,
    processed_fpl: &mut FetchedPlaylist<'_>,
) {
    // Skip-flag is the kill-switch: no resolve, no probe, no iteration.
    if processed_fpl.input.has_flag(ConfigInputFlags::XtreamSkipSeries) {
        return;
    }

    let mut resolve_options = get_resolve_series_options(target, processed_fpl);

    if !ctx.config.is_ffprobe_enabled().await {
        resolve_options.unset_flag(ResolveOptionsFlags::Probe);
    }
    provider_fpl.source.release_resources(XtreamCluster::Series);

    playlist_resolve_series_info(ctx, errors, processed_fpl, resolve_options, pipe, target).await;

    if provider_fpl.is_memory() {
        sync_resolved_series_properties(provider_fpl, processed_fpl);
    }

    provider_fpl.source.obtain_resources().await;
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn playlist_resolve_series_info(
    ctx: &PlaylistProcessingContext,
    _errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
    resolve_options: ResolveOptions,
    pipe: &ProcessingPipe,
    target: &ConfigTarget,
) {
    let filter = |pli: &PlaylistItem| {
        if pli.header.xtream_cluster != XtreamCluster::Series || pli.header.item_type != PlaylistItemType::SeriesInfo {
            return false;
        }
        true
    };

    // Skip if nothing to do
    let skip_resolve = !resolve_options.has_flag(ResolveOptionsFlags::Resolve)
        && !resolve_options.has_flag(ResolveOptionsFlags::Probe)
        && !resolve_options.has_flag(ResolveOptionsFlags::TmdbMissing);

    let resolve_tmdb_enabled = fpl.input.has_flag(ConfigInputFlags::ResolveTmdb);

    let groups_to_add = if resolve_options.has_flag(ResolveOptionsFlags::Background) && ctx.metadata_manager.is_some() {
        queue_background_series_info(ctx, fpl, filter, &resolve_options, skip_resolve, resolve_tmdb_enabled)
    } else {
        process_immediate_series_info(ctx, fpl, filter, &resolve_options, skip_resolve, resolve_tmdb_enabled).await
    };

    // Apply pipe transformations to new groups
    let mut new_playlist = groups_to_add;
    for f in pipe {
        let mut source = MemoryPlaylistSource::new(new_playlist);
        if let Some(v) = f(&mut source, target) {
            new_playlist = v;
        } else {
            new_playlist = source.take_groups();
        }
    }

    // Apply resolved episodes to playlist
    for group in new_playlist {
        fpl.update_playlist(&group).await;
    }
}

fn sync_resolved_series_properties(provider_fpl: &mut FetchedPlaylist<'_>, processed_fpl: &mut FetchedPlaylist<'_>) {
    let mut resolved_series_by_provider_id: HashMap<u32, SeriesStreamProperties> = HashMap::new();

    for pli in processed_fpl.items() {
        if pli.header.xtream_cluster != XtreamCluster::Series || pli.header.item_type != PlaylistItemType::SeriesInfo {
            continue;
        }

        let Some(provider_id) = pli.get_provider_id() else {
            continue;
        };
        if provider_id == 0 {
            continue;
        }

        if let Some(StreamProperties::Series(properties)) = pli.header.additional_properties.as_ref() {
            resolved_series_by_provider_id.entry(provider_id).or_insert_with(|| properties.as_ref().clone());
        }
    }

    if resolved_series_by_provider_id.is_empty() {
        return;
    }

    for source_pli in provider_fpl.items_mut() {
        if source_pli.header.xtream_cluster != XtreamCluster::Series
            || source_pli.header.item_type != PlaylistItemType::SeriesInfo
        {
            continue;
        }

        let Some(provider_id) = source_pli.get_provider_id() else {
            continue;
        };
        if provider_id == 0 {
            continue;
        }

        if let Some(resolved) = resolved_series_by_provider_id.get(&provider_id) {
            source_pli.header.additional_properties = Some(StreamProperties::Series(Box::new(resolved.clone())));
        }
    }
}

fn queue_background_series_info(
    ctx: &PlaylistProcessingContext,
    fpl: &mut FetchedPlaylist<'_>,
    filter: impl Fn(&PlaylistItem) -> bool,
    resolve_options: &ResolveOptions,
    skip_resolve: bool,
    resolve_tmdb_enabled: bool,
) -> Vec<PlaylistGroup> {
    let mut groups_to_add = Vec::new();
    let Some(mgr) = ctx.metadata_manager.as_ref() else {
        return groups_to_add;
    };

    let input = fpl.input;
    let input_name_arc = input.name.clone();

    for pli in fpl.items_mut() {
        if !filter(pli) {
            continue;
        }

        let provider_id = if let Ok(uid) = pli.header.id.parse::<u32>() {
            ProviderIdType::Id(uid)
        } else {
            ProviderIdType::from(pli.header.id.to_string())
        };

        if !skip_resolve {
            let reasons = check_resolve_reasons(resolve_options, resolve_tmdb_enabled, pli);

            if !reasons.is_empty() {
                let task = UpdateTask::ResolveSeries {
                    id: provider_id,
                    reason: reasons,
                    delay: resolve_options.resolve_delay,
                };
                mgr.queue_task_background(input_name_arc.clone(), task);
            }
        }

        if let Some(group_obj) = expand_series_item(pli, input) {
            groups_to_add.push(group_obj);
        }
    }
    groups_to_add
}

#[allow(clippy::too_many_lines)]
async fn process_immediate_series_info(
    ctx: &PlaylistProcessingContext,
    fpl: &mut FetchedPlaylist<'_>,
    filter: impl Fn(&PlaylistItem) -> bool,
    resolve_options: &ResolveOptions,
    skip_resolve: bool,
    resolve_tmdb_enabled: bool,
) -> Vec<PlaylistGroup> {
    let input = fpl.input;
    let working_dir = &ctx.config.config.load().working_dir;
    let storage_path = match get_input_storage_path(&input.name, working_dir).await {
        Ok(path) => path,
        Err(err) => {
            error!("Can't resolve series, input storage directory for input '{}' failed: {err}", input.name);
            return Vec::new();
        }
    };

    // Keep an optional read query open and reopen lazily only when needed.
    let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Series);
    let mut db_query_holder: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>> = None;
    let mut _db_lock_holder = None;

    let mut groups_to_add = Vec::new();
    let mut batch: Vec<(ProviderIdType, SeriesStreamProperties)> = Vec::with_capacity(BATCH_SIZE);
    let mut retry_once_ids: HashSet<ProviderIdType> = HashSet::new();
    let mut processed_count = 0;
    let mut last_log_time = Instant::now();
    let series_probe_settings = {
        let config = ctx.config.config.load();
        SeriesProbeSettings::from_metadata_update(config.metadata_update.as_ref())
    };

    for pli in fpl.items_mut() {
        if !filter(pli) {
            continue;
        }

        let provider_id = if let Ok(uid) = pli.header.id.parse::<u32>() {
            ProviderIdType::Id(uid)
        } else {
            ProviderIdType::from(&*pli.header.id)
        };
        let mut defer_expand = false;

        if !skip_resolve {
            let reasons = check_resolve_reasons(resolve_options, resolve_tmdb_enabled, pli);

            if !reasons.is_empty() {
                if let Some(active_provider) = ctx.provider_manager.as_ref() {
                    if db_query_holder.is_none() && xtream_path.exists() {
                        let file_lock = ctx.config.file_locks.read_lock(&xtream_path).await;
                        let xtream_path = xtream_path.clone();
                        let query = match tokio::task::spawn_blocking(move || {
                            BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)
                        })
                        .await
                        {
                            Ok(Ok(query)) => Some((query, file_lock)),
                            Ok(Err(err)) => {
                                error!("Failed to open BPlusTreeQuery for Series: {err}");
                                None
                            }
                            Err(err) => {
                                error!("Failed to open BPlusTreeQuery for Series: {err}");
                                None
                            }
                        };

                        if let Some((query, guard)) = query {
                            db_query_holder = Some(Arc::new(Mutex::new(query)));
                            _db_lock_holder = Some(guard);
                        }
                    }
                    // Pass the optional reference to the query
                    let db_query_ref = db_query_holder.as_ref().map(Arc::clone);

                    match update_series_info_immediate(
                        ctx,
                        active_provider,
                        input,
                        pli,
                        provider_id.clone(),
                        &reasons,
                        db_query_ref,
                        series_probe_settings,
                    )
                    .await
                    {
                        Ok(Some(updated_props)) => {
                            pli.header.additional_properties =
                                Some(StreamProperties::Series(Box::new(updated_props.clone())));
                            batch.push((provider_id.clone(), updated_props));

                            if batch.len() >= BATCH_SIZE {
                                // Release lock before persisting to avoid deadlock (persist needs write lock)
                                db_query_holder = None;
                                _db_lock_holder = None;

                                let updates: Vec<(u32, SeriesStreamProperties)> = batch
                                    .iter()
                                    .filter_map(|(id, props)| {
                                        if let ProviderIdType::Id(vid) = id {
                                            Some((*vid, props.clone()))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();

                                if updates.is_empty() {
                                    batch.clear();
                                } else {
                                    match persist_input_series_info_batch(
                                        &ctx.config,
                                        &storage_path,
                                        XtreamCluster::Series,
                                        &input.name,
                                        updates,
                                    )
                                    .await
                                    {
                                        Ok(()) => batch.clear(),
                                        Err(err) => {
                                            error!(
                                                "persist_input_series_info_batch failed for XtreamCluster::Series on input '{}'. batch.clear() skipped. Error: {}",
                                                input.name, err
                                            );
                                        }
                                    }
                                }
                            }

                            processed_count += 1;
                            if resolve_options.resolve_delay > 0 {
                                tokio::time::sleep(Duration::from_secs(u64::from(resolve_options.resolve_delay))).await;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            error!("Failed to update Series metadata for {}: {e}", pli.header.title);
                            retry_once_ids.insert(provider_id.clone());
                            defer_expand = true;
                        }
                    }
                }
            }
        }

        if !defer_expand {
            if let Some(group_obj) = expand_series_item(pli, input) {
                groups_to_add.push(group_obj);
            }
        }

        if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
            info!("resolved {processed_count} series info");
            last_log_time = Instant::now();
        }
    }

    if !retry_once_ids.is_empty() {
        let retry_delay_secs = u64::from(resolve_options.resolve_delay).max(FOREGROUND_MIN_RETRY_DELAY_SECS);
        process_foreground_retry_once!(
            ctx: ctx,
            fpl: fpl,
            filter: filter,
            retry_once_ids: retry_once_ids,
            retry_delay_secs: retry_delay_secs,
            xtream_path: xtream_path,
            db_query_holder: db_query_holder,
            db_lock_holder: _db_lock_holder,
            batch: batch,
            batch_size: BATCH_SIZE,
            retry_batch_max_len: RETRY_BATCH_MAX_SIZE,
            processed_count: processed_count,
            query_error_context: "Series retry",
            reasons: |retry_pli| {
                if skip_resolve {
                    ResolveReasonSet::new()
                } else {
                    check_resolve_reasons(resolve_options, resolve_tmdb_enabled, retry_pli)
                }
            },
            update: |active_provider, retry_pli, provider_id, reasons, db_query_ref| update_series_info_immediate(
                ctx,
                active_provider,
                input,
                retry_pli,
                provider_id,
                reasons,
                db_query_ref,
                series_probe_settings,
            ),
            apply_properties: |retry_pli, updated_props| {
                retry_pli.header.additional_properties =
                    Some(StreamProperties::Series(Box::new(updated_props.clone())));
            },
            persist: |updates| persist_input_series_info_batch(
                &ctx.config,
                &storage_path,
                XtreamCluster::Series,
                &input.name,
                updates,
            ),
            on_persist_error: |err| {
                error!("persist_input_series_info_batch failed for Series retry on input '{}'. Error: {}", input.name, err);
            },
            on_retry_error: |retry_pli, err| {
                error!("Foreground retry failed for Series {}: {err}", retry_pli.header.title);
            },
            on_after_attempt: |retry_pli, _retry_succeeded| {
                if let Some(group_obj) = expand_series_item(retry_pli, input) {
                    groups_to_add.push(group_obj);
                }
            },
        );
    }

    if !batch.is_empty() {
        // Release lock before final persist
        _db_lock_holder = None;

        let updates: Vec<(u32, SeriesStreamProperties)> = batch
            .into_iter()
            .filter_map(|(id, props)| if let ProviderIdType::Id(vid) = id { Some((vid, props)) } else { None })
            .collect();

        if !updates.is_empty() {
            if let Err(err) =
                persist_input_series_info_batch(&ctx.config, &storage_path, XtreamCluster::Series, &input.name, updates)
                    .await
            {
                error!("Failed to persist final batch series info: {err}");
            }
        }
    }

    if processed_count > 0 {
        info!("Processed {processed_count} series info");
    }

    groups_to_add
}

fn expand_series_item(pli: &PlaylistItem, input: &ConfigInput) -> Option<PlaylistGroup> {
    if let Some(StreamProperties::Series(properties)) = pli.header.additional_properties.as_ref() {
        let global_release_date = properties.release_date.clone();
        let (group, series_name) = {
            let header = &pli.header;
            (header.group.clone(), header.get_name())
        };

        if let Some(episodes) = parse_xtream_series_info(
            &pli.get_uuid(),
            properties,
            &group,
            &series_name,
            input,
            global_release_date.as_ref(),
            pli.header.source_ordinal,
        ) {
            return Some(PlaylistGroup {
                id: pli.header.category_id,
                title: pli.header.group.clone(),
                channels: episodes,
                xtream_cluster: XtreamCluster::Series,
            });
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn update_series_info_immediate(
    ctx: &PlaylistProcessingContext,
    active_provider: &Arc<ActiveProviderManager>,
    input: &ConfigInput,
    pli: &PlaylistItem,
    id: ProviderIdType,
    reasons: &ResolveReasonSet,
    db_query: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>>,
    probe_settings: SeriesProbeSettings,
) -> Result<Option<SeriesStreamProperties>, TuliproxError> {
    let fetch_info = reasons.contains(ResolveReason::Info);
    let resolve_tmdb = reasons.contains(ResolveReason::Tmdb) || reasons.contains(ResolveReason::Date);

    update_series_metadata(
        &ctx.config,
        &ctx.client,
        input,
        id,
        active_provider,
        None, // active_handle
        Some(&pli.header.title),
        false, // save (we batch in caller)
        fetch_info,
        resolve_tmdb,
        reasons.contains(ResolveReason::Probe),
        probe_settings,
        db_query,
    )
    .await
}

fn check_resolve_reasons(
    resolve_options: &ResolveOptions,
    resolve_tmdb_enabled: bool,
    pli: &PlaylistItem,
) -> ResolveReasonSet {
    let mut reasons = ResolveReasonSet::new();

    check_needs_info(resolve_options, pli, &mut reasons);
    check_resolve_tmdb(resolve_options, resolve_tmdb_enabled, pli, &mut reasons);

    if resolve_options.has_flag(ResolveOptionsFlags::Probe) {
        check_needs_probe(pli, &mut reasons);
    }
    reasons
}

fn check_needs_info(resolve_options: &ResolveOptions, pli: &PlaylistItem, reasons: &mut ResolveReasonSet) {
    let needs_info = resolve_options.has_flag(ResolveOptionsFlags::Resolve) && !pli.has_details();
    if needs_info {
        reasons.set(ResolveReason::Info);
    }
}

fn check_resolve_tmdb(
    resolve_options: &ResolveOptions,
    resolve_tmdb_enabled: bool,
    pli: &PlaylistItem,
    reasons: &mut ResolveReasonSet,
) {
    if resolve_tmdb_enabled && resolve_options.has_flag(ResolveOptionsFlags::TmdbMissing) {
        let (has_tmdb, has_date, title_present) = match pli.header.additional_properties.as_ref() {
            Some(StreamProperties::Series(series_stream_props)) => (
                series_stream_props.tmdb.is_some(),
                series_stream_props.release_date.is_some(),
                !series_stream_props.name.is_empty() || !pli.header.title.is_empty(),
            ),
            None => (false, false, !pli.header.title.is_empty()),
            _ => return,
        };

        if title_present && (!has_tmdb || !has_date) {
            if !has_tmdb {
                reasons.set(ResolveReason::Tmdb);
            }
            if !has_date {
                reasons.set(ResolveReason::Date);
            }
        }
    }
}

fn check_needs_probe(pli: &PlaylistItem, reasons: &mut ResolveReasonSet) {
    let needs_probe = match pli.header.additional_properties.as_ref() {
        Some(StreamProperties::Series(series_stream_props)) => {
            series_stream_props.details.as_ref().and_then(|d| d.episodes.as_ref()).is_some_and(|episodes| {
                episodes.iter().any(|ep| {
                    let missing_video = !MediaQuality::is_valid_media_info(ep.video.as_deref());
                    let missing_audio = !MediaQuality::is_valid_media_info(ep.audio.as_deref());
                    missing_video || missing_audio
                })
            })
        }
        _ => false,
    };

    if needs_probe {
        reasons.set(ResolveReason::Probe);
    }
}
/// Updates metadata for a single Series (Info + Episodes Probe) and persists it.
///
/// # Arguments
/// * `save` - If true, persists changes to the input database immediately (Instant strategy).
///   If false, returns the properties so the caller can batch persist them (Bundled strategy).
/// * `fetch_info` - If true, fetches details from Provider API. If false, uses existing/dummy data.
/// * `resolve_tmdb` - If true, resolves missing TMDB/date metadata from available titles.
/// * `db_query` - Optional pre-opened DB handle to avoid re-opening file.
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::fn_params_excessive_bools)]
pub async fn update_series_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    id: ProviderIdType,
    active_provider: &Arc<ActiveProviderManager>,
    active_handle: Option<&ProviderHandle>,
    playlist_title: Option<&str>,
    save: bool,
    fetch_info: bool,
    resolve_tmdb: bool,
    do_probe: bool,
    probe_settings: SeriesProbeSettings,
    db_query: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>>,
) -> Result<Option<SeriesStreamProperties>, TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir)
        .await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    if input.has_flag(ConfigInputFlags::XtreamSkipSeries) {
        return Ok(None);
    }

    // Try to load existing info first
    let mut props: Option<SeriesStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;

    let series_id_opt = if let ProviderIdType::Id(vid) = id { Some(vid) } else { None };

    if let Some(series_id) = series_id_opt {
        if let Some(query) = db_query {
            let query = Arc::clone(&query);
            let item = match tokio::task::spawn_blocking(move || {
                let mut guard = query.lock();
                guard.query_zero_copy(&series_id)
            })
            .await
            {
                Ok(Ok(item)) => item,
                Ok(Err(err)) => {
                    error!("Failed to query Series metadata from disk for {series_id}: {err}");
                    None
                }
                Err(err) => {
                    error!("Failed to query Series metadata from disk for {series_id}: {err}");
                    None
                }
            };

            if let Some(item) = item {
                existing_item = Some(item.clone());
                if let Some(StreamProperties::Series(p)) = item.additional_properties.as_ref() {
                    props = Some(*p.clone());
                }
            }
        } else {
            let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Series);
            if xtream_path.exists() {
                let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
                let xtream_path = xtream_path.clone();
                let item = match tokio::task::spawn_blocking(move || {
                    let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)?;
                    query.query_zero_copy(&series_id)
                })
                .await
                {
                    Ok(Ok(item)) => item,
                    Ok(Err(err)) => {
                        error!("Failed to query Series metadata from disk for {series_id}: {err}");
                        None
                    }
                    Err(err) => {
                        error!("Failed to query Series metadata from disk for {series_id}: {err}");
                        None
                    }
                };

                if let Some(item) = item {
                    existing_item = Some(item.clone());
                    if let Some(StreamProperties::Series(p)) = item.additional_properties.as_ref() {
                        props = Some(*p.clone());
                    }
                }
            }
        }
    }

    let mut fetched_new = false;
    let mut properties_updated = false;

    let display_id = series_id_opt.map_or_else(|| "StringID".to_string(), |v| v.to_string());

    // Input DB is canonical, but if details are missing we still need one info fetch to complete the record.
    let should_fetch_info = fetch_info && props.as_ref().is_none_or(|p| p.details.is_none());

    // 1. Fetch Info from Provider (only when missing in input DB)
    if should_fetch_info {
        if let Some(series_id) = series_id_opt {
            // Fetch Info from Provider
            let info_url = xtream::get_xtream_player_api_info_url(input, XtreamCluster::Series, series_id)
                .ok_or_else(|| shared::error::info_err!("Failed to build info URL"))?;

            let input_source = InputSource::from(input).with_url(info_url);
            let content = xtream::get_xtream_stream_info_content(app_config, client, &input_source, true)
                .await
                .map_err(|e| shared::error::info_err!("{e}"))?;

            if !content.is_empty() {
                if let Ok(mut json_value) = serde_json::from_str::<Value>(&content) {
                    if let Some(info) = json_value.get_mut("info").and_then(|v| v.as_object_mut()) {
                        crate::model::normalize_release_date(info);
                    }

                    if let Ok(info) = serde_json::from_value::<XtreamSeriesInfo>(json_value) {
                        if let Some(existing) = &existing_item {
                            props = Some(SeriesStreamProperties::from_info(&info, existing));
                            fetched_new = true;
                            properties_updated = true;
                        }
                    }
                }
            }
        }
    }

    // If we don't have props, verify if we can proceed with minimal dummy props
    if props.is_none() {
        if let Some(title) = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref())) {
            let new_props = SeriesStreamProperties {
                name: title.into(),
                series_id: series_id_opt.unwrap_or(0),
                ..Default::default()
            };
            props = Some(new_props);
        } else {
            return Err(shared::error::info_err!("No Series properties available and no title found for {display_id}"));
        }
    }

    let mut properties = props.ok_or_else(|| {
        shared::error::info_err!("No Series properties available after fallback creation for {display_id}")
    })?;

    let resolve_tmdb_enabled = input.has_flag(ConfigInputFlags::ResolveTmdb);

    // 2. Resolve TMDB/Date if missing
    if resolve_tmdb
        && resolve_tmdb_enabled
        && (properties.tmdb.is_none() || properties.release_date.is_none())
        && !properties.name.is_empty()
    {
        let config = app_config.config.load();
        let library_config = config.library.as_ref();
        let meta_resolver = MetadataResolver::new(library_config, client.clone());

        let mut meta = None;
        let mut tried_title = false;

        // Try extracting year locally first
        if properties.release_date.is_none() {
            let meta_parse = ptt_parse_title(&properties.name);
            if let Some(year) = meta_parse.year {
                properties.release_date = Some(format!("{year}-01-01").into());
                properties_updated = true;
                debug_if_enabled!("Parsed local year for Series '{}': {}", properties.name, year);
            }
        }

        // 1. & 2. Playlist Title
        let title_candidate = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()));
        if let Some(title) = title_candidate {
            if !title.is_empty() {
                trace!("Resolving TMDB for Series using Playlist Title '{title}' (ID: {display_id})...");
                meta = meta_resolver.resolve_from_title(title, properties.tmdb, false, resolve_tmdb_enabled).await;
                tried_title = true;
            }
        }

        // 3. API Name (fallback)
        if (meta.is_none() || (meta.as_ref().is_some_and(|m| m.tmdb_id().is_none()))) && !properties.name.is_empty() {
            let title_already_tried = if let Some(t) = title_candidate { t == properties.name.as_ref() } else { false };
            if !tried_title || !title_already_tried {
                debug!("Fallback to API Name '{}'...", properties.name);
                meta = meta_resolver
                    .resolve_from_title(&properties.name, properties.tmdb, false, resolve_tmdb_enabled)
                    .await;
            }
        }

        if let Some(m) = meta {
            if properties.tmdb.is_none() {
                properties.tmdb = m.tmdb_id();
                properties_updated = true;
            }
            if properties.release_date.is_none() {
                properties.release_date = m.year().map(|y| format!("{y}-01-01").into());
                properties_updated = true;
            }
            if properties_updated {
                let id_display = properties.tmdb.map_or("None".to_string(), |id| id.to_string());
                debug_if_enabled!("Resolved TMDB for Series ID {}: {}", display_id, id_display);
            }
        }
    }

    // 3. Probe Episodes (if enabled)
    if do_probe && app_config.is_ffprobe_enabled().await {
        if let Some(details) = properties.details.as_mut() {
            if let Some(episodes) = details.episodes.as_mut() {
                let config = app_config.config.load();
                let user_agent = config.default_user_agent.clone();

                let input_url = input.url.as_str();
                let input_username = input.username.as_deref().unwrap_or("");
                let input_password = input.password.as_deref().unwrap_or("");

                let mut probed_count = 0;
                let mut missing_any = false;

                for ep in episodes {
                    let missing_video = !MediaQuality::is_valid_media_info(ep.video.as_deref());
                    let missing_audio = !MediaQuality::is_valid_media_info(ep.audio.as_deref());

                    if missing_video || missing_audio {
                        missing_any = true;

                        // Acquire Connection logic
                        let temp_handle = if active_handle.is_some() {
                            None
                        } else {
                            active_provider.acquire_connection_for_probe(&input.name).await
                        };

                        if active_handle.is_some() || temp_handle.is_some() {
                            let episode_url =
                                create_xtream_series_episode_url(input_url, input_username, input_password, ep);

                            // Specific logging for the user to follow
                            let missing_reason = if missing_video && missing_audio {
                                "video/audio"
                            } else if missing_video {
                                "video"
                            } else {
                                "audio"
                            };
                            debug!(
                                "Probing Series Episode '{}' (S{}E{}) - Missing {}",
                                ep.title, ep.season, ep.episode_num, missing_reason
                            );

                            if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                                &episode_url,
                                user_agent.as_deref(),
                                probe_settings.analyze_duration_micros,
                                probe_settings.probe_size_bytes,
                                probe_settings.timeout_secs,
                            )
                            .await
                            {
                                if let Some(v) = raw_video {
                                    ep.video = Some(v.to_string().into());
                                    properties_updated = true;
                                }
                                if let Some(a) = raw_audio {
                                    ep.audio = Some(a.to_string().into());
                                    properties_updated = true;
                                }
                                probed_count += 1;
                            }

                            if let Some(h) = temp_handle {
                                active_provider.release_handle(&h).await;
                            }
                        } else {
                            warn!("Skipping probe for series episode {} due to connection limits", ep.title);
                        }
                    }
                }
                if probed_count > 0 {
                    info!("Probed {probed_count} episodes for Series ID {display_id}");
                } else if !missing_any {
                    debug!("Series probe skipped for ID {display_id} (all episodes already have A/V details)");
                }
            } else {
                debug!("Series probe skipped for ID {display_id} (no episode details available)");
            }
        } else {
            debug!("Series probe skipped for ID {display_id} (no series details available)");
        }
    }

    // 4. Persist
    if properties_updated || fetched_new {
        if save {
            if let Some(series_id) = series_id_opt {
                persists_input_series_info(
                    app_config,
                    &storage_path,
                    XtreamCluster::Series,
                    &input.name,
                    series_id,
                    &properties,
                )
                .await
                .map_err(|e| shared::error::info_err!("Persist error: {e}"))?;

                debug_if_enabled!("Successfully updated Series metadata for ID {}", series_id);
            }
        }
        return Ok(Some(properties));
    }

    Ok(None)
}
