use crate::api::model::{ActiveProviderManager, ProviderHandle};
use crate::api::model::{ProviderIdType, ResolveReason, ResolveReasonSet, UpdateTask};
use crate::library::MetadataResolver;
use crate::model::FetchedPlaylist;
use crate::model::InputSource;
use crate::model::{AppConfig, ConfigTarget};
use crate::model::{ConfigInput, ConfigInputFlags};
use crate::processing::processor::playlist::PlaylistProcessingContext;
use crate::processing::processor::{
    create_resolve_options_function_for_xtream_target, process_foreground_retry_once, ResolveOptions,
    ResolveOptionsFlags, FOREGROUND_BATCH_SIZE as BATCH_SIZE, FOREGROUND_MIN_RETRY_DELAY_SECS,
    FOREGROUND_RETRY_BATCH_MAX_SIZE as RETRY_BATCH_MAX_SIZE,
};
use crate::ptt::ptt_parse_title;
use crate::repository::get_input_storage_path;
use crate::repository::persist_input_vod_info;
use crate::repository::persist_input_vod_info_batch;
use crate::repository::{xtream_get_file_path, BPlusTreeQuery};
use crate::utils::{debug_if_enabled, trace_if_enabled, xtream};
use log::{debug, error, info, log_enabled, trace, warn, Level};
use parking_lot::Mutex;
use serde_json::Value;
use shared::error::TuliproxError;
use shared::model::{
    MediaQuality, PlaylistEntry, PlaylistItem, PlaylistItemType, StreamProperties, VideoStreamDetailProperties,
    VideoStreamProperties, XtreamCluster, XtreamPlaylistItem, XtreamVideoInfo,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

create_resolve_options_function_for_xtream_target!(vod);

#[allow(clippy::too_many_lines)]
pub async fn playlist_resolve_vod(
    ctx: &PlaylistProcessingContext,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    provider_fpl: &mut FetchedPlaylist<'_>,
    fpl: &mut FetchedPlaylist<'_>,
) {
    // Skip-flag is the kill-switch: no resolve, no probe, no iteration.
    if fpl.input.has_flag(ConfigInputFlags::XtreamSkipVod) {
        return;
    }

    let resolve_options = get_resolve_vod_options(target, fpl);

    let app_config: &Arc<AppConfig> = &ctx.config;
    let do_probe = resolve_options.has_flag(ResolveOptionsFlags::Probe) && app_config.is_ffprobe_enabled().await;

    // Determine if we need to do anything
    if !resolve_options.has_flag(ResolveOptionsFlags::Resolve)
        && !do_probe
        && !resolve_options.has_flag(ResolveOptionsFlags::TmdbMissing)
    {
        return;
    }

    provider_fpl.source.release_resources(XtreamCluster::Video);

    playlist_resolve_vod_info(ctx, errors, fpl, resolve_options, do_probe).await;

    if provider_fpl.is_memory() {
        sync_resolved_vod_properties(provider_fpl, fpl);
    }

    provider_fpl.source.obtain_resources().await;
}

fn sync_resolved_vod_properties(provider_fpl: &mut FetchedPlaylist<'_>, processed_fpl: &mut FetchedPlaylist<'_>) {
    let mut resolved_vod_by_provider_id: HashMap<u32, VideoStreamProperties> = HashMap::new();

    for pli in processed_fpl.items() {
        if pli.header.xtream_cluster != XtreamCluster::Video || pli.header.item_type != PlaylistItemType::Video {
            continue;
        }

        let Some(provider_id) = pli.get_provider_id() else {
            continue;
        };
        if provider_id == 0 {
            continue;
        }

        if let Some(StreamProperties::Video(properties)) = pli.header.additional_properties.as_ref() {
            resolved_vod_by_provider_id.entry(provider_id).or_insert_with(|| properties.as_ref().clone());
        }
    }

    if resolved_vod_by_provider_id.is_empty() {
        return;
    }

    for source_pli in provider_fpl.items_mut() {
        if source_pli.header.xtream_cluster != XtreamCluster::Video
            || source_pli.header.item_type != PlaylistItemType::Video
        {
            continue;
        }

        let Some(provider_id) = source_pli.get_provider_id() else {
            continue;
        };
        if provider_id == 0 {
            continue;
        }

        if let Some(resolved) = resolved_vod_by_provider_id.get(&provider_id) {
            source_pli.header.additional_properties = Some(StreamProperties::Video(Box::new(resolved.clone())));
        }
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn playlist_resolve_vod_info(
    ctx: &PlaylistProcessingContext,
    _errors: &mut Vec<TuliproxError>,
    fpl: &mut FetchedPlaylist<'_>,
    resolve_options: ResolveOptions,
    do_probe: bool,
) {
    let filter = |pli: &PlaylistItem| {
        if pli.header.xtream_cluster != XtreamCluster::Video || pli.header.item_type != PlaylistItemType::Video {
            return false;
        }
        true
    };

    let resolve_tmdb_enabled = fpl.input.has_flag(ConfigInputFlags::ResolveTmdb);

    if resolve_options.has_flag(ResolveOptionsFlags::Background) && ctx.metadata_manager.is_some() {
        queue_background_vod_info(ctx, fpl, filter, &resolve_options, do_probe, resolve_tmdb_enabled);
    } else {
        process_immediate_vod_info(ctx, fpl, filter, resolve_options, do_probe, resolve_tmdb_enabled).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn process_immediate_vod_info(
    ctx: &PlaylistProcessingContext,
    fpl: &mut FetchedPlaylist<'_>,
    filter: impl Fn(&PlaylistItem) -> bool,
    resolve_options: ResolveOptions,
    do_probe: bool,
    resolve_tmdb_enabled: bool,
) {
    let input = fpl.input;
    let working_dir = &ctx.config.config.load().working_dir;

    let storage_path = match get_input_storage_path(&input.name, working_dir).await {
        Ok(path) => path,
        Err(err) => {
            error!("Can't resolve vod, input storage directory for input '{}' failed: {err}", input.name);
            return;
        }
    };

    // Keep an optional read query open and reopen lazily only when needed.
    let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Video);
    let mut db_query_holder: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>> = None;
    let mut _db_lock_holder = None;

    let mut batch: Vec<(ProviderIdType, VideoStreamProperties)> = Vec::with_capacity(BATCH_SIZE);
    let mut retry_once_ids: HashSet<ProviderIdType> = HashSet::new();
    let mut processed_count = 0;
    let mut last_log_time = Instant::now();

    for pli in fpl.items_mut() {
        if !filter(pli) {
            continue;
        }

        let provider_id = if let Ok(uid) = pli.header.id.parse::<u32>() {
            ProviderIdType::Id(uid)
        } else {
            ProviderIdType::from(&*pli.header.id)
        };

        let reasons = check_resolve_reasons(&resolve_options, do_probe, resolve_tmdb_enabled, pli);

        if !reasons.is_empty() {
            // Path 2: Internal Update (Inline)
            if let Some(prov_mgr) = ctx.provider_manager.as_ref() {
                if db_query_holder.is_none() && xtream_path.exists() {
                    let lock = ctx.config.file_locks.read_lock(&xtream_path).await;
                    let xtream_path = xtream_path.clone();
                    let query = match tokio::task::spawn_blocking(move || {
                        BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)
                    })
                    .await
                    {
                        Ok(Ok(query)) => Some(query),
                        Ok(Err(err)) => {
                            error!("Failed to open BPlusTreeQuery for VOD: {err}");
                            None
                        }
                        Err(err) => {
                            error!("Failed to open BPlusTreeQuery for VOD: {err}");
                            None
                        }
                    };

                    if let Some(query) = query {
                        db_query_holder = Some(Arc::new(Mutex::new(query)));
                        _db_lock_holder = Some(lock);
                    }
                }
                // Pass the optional reference to the query
                let db_query_ref = db_query_holder.as_ref().map(Arc::clone);

                match update_vod_info_immediate(ctx, prov_mgr, input, pli, provider_id.clone(), &reasons, db_query_ref)
                    .await
                {
                    Ok(Some(updated_props)) => {
                        // Update the current item in fpl
                        pli.header.additional_properties =
                            Some(StreamProperties::Video(Box::new(updated_props.clone())));
                        batch.push((provider_id.clone(), updated_props));
                        if batch.len() >= BATCH_SIZE {
                            // Release lock before persisting to avoid deadlock (persist needs write lock)
                            db_query_holder = None;
                            _db_lock_holder = None;

                            // Filter for u32 IDs for persistence
                            let updates: Vec<(u32, VideoStreamProperties)> = batch
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
                                match persist_input_vod_info_batch(
                                    &ctx.config,
                                    &storage_path,
                                    XtreamCluster::Video,
                                    &input.name,
                                    updates,
                                )
                                .await
                                {
                                    Ok(()) => batch.clear(),
                                    Err(err) => {
                                        error!(
                                            "persist_input_vod_info_batch failed for XtreamCluster::Video on input '{}'. batch.clear() skipped. Error: {err}",
                                            input.name
                                        );
                                    }
                                }
                            }
                        }

                        processed_count += 1;

                        // Resolve delay for inline execution to prevent flooding provider
                        // Only delay if we actually did something (reason is not empty) - logic implies we did
                        if resolve_options.resolve_delay > 0 {
                            tokio::time::sleep(Duration::from_secs(u64::from(resolve_options.resolve_delay))).await;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        error!("Failed to update VOD metadata for {}: {e}", pli.header.title);
                        retry_once_ids.insert(provider_id.clone());
                    }
                }
            }
        }

        if log_enabled!(Level::Info) && last_log_time.elapsed().as_secs() >= 30 {
            info!("resolved {processed_count} vod info");
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
            query_error_context: "VOD retry",
            reasons: |retry_pli| check_resolve_reasons(&resolve_options, do_probe, resolve_tmdb_enabled, retry_pli),
            update: |active_provider, retry_pli, provider_id, reasons, db_query_ref| update_vod_info_immediate(
                ctx,
                active_provider,
                input,
                retry_pli,
                provider_id,
                reasons,
                db_query_ref,
            ),
            apply_properties: |retry_pli, updated_props| {
                retry_pli.header.additional_properties = Some(StreamProperties::Video(Box::new(updated_props.clone())));
            },
            persist: |updates| persist_input_vod_info_batch(
                &ctx.config,
                &storage_path,
                XtreamCluster::Video,
                &input.name,
                updates,
            ),
            on_persist_error: |err| {
                error!("persist_input_vod_info_batch failed for VOD retry on input '{}'. Error: {err}", input.name);
            },
            on_retry_error: |retry_pli, err| {
                error!("Foreground retry failed for VOD {}: {err}", retry_pli.header.title);
            },
            on_after_attempt: |_retry_pli, _retry_succeeded| {},
        );
    }

    // Flush the remaining batch if bundled strategy
    if !batch.is_empty() {
        // Release lock before final persist
        _db_lock_holder = None;

        let updates: Vec<(u32, VideoStreamProperties)> = batch
            .into_iter()
            .filter_map(|(id, props)| if let ProviderIdType::Id(vid) = id { Some((vid, props)) } else { None })
            .collect();

        if !updates.is_empty() {
            if let Err(err) =
                persist_input_vod_info_batch(&ctx.config, &storage_path, XtreamCluster::Video, &input.name, updates)
                    .await
            {
                error!("Failed to persist final batch VOD info: {err}");
            }
        }
    }

    if processed_count > 0 {
        info!("Processed {processed_count} vod info");
    }
}

fn check_resolve_reasons(
    resolve_options: &ResolveOptions,
    do_probe: bool,
    resolve_tmdb_enabled: bool,
    pli: &PlaylistItem,
) -> ResolveReasonSet {
    // Check if we need to do anything for this item
    let mut reasons = ResolveReasonSet::default();

    check_needs_info(resolve_options, pli, &mut reasons);
    // TMDB check
    check_resolve_tmdb(resolve_options, resolve_tmdb_enabled, pli, &mut reasons);

    // Probe check (independent of resolve info fetch; we still probe when A/V details are missing)
    if do_probe {
        check_needs_probe(pli, &mut reasons);
    }
    reasons
}

fn check_needs_probe(pli: &PlaylistItem, reasons: &mut ResolveReasonSet) {
    let needs_probe = match pli.header.additional_properties.as_ref() {
        Some(StreamProperties::Video(props)) => {
            let details = props.details.as_ref();
            let missing_video = !MediaQuality::is_valid_media_info(details.and_then(|d| d.video.as_deref()));
            let missing_audio = !MediaQuality::is_valid_media_info(details.and_then(|d| d.audio.as_deref()));
            missing_video || missing_audio
        }
        None => true,
        _ => false,
    };
    if needs_probe {
        reasons.set(ResolveReason::Probe);
    }
}

fn check_needs_info(resolve_options: &ResolveOptions, pli: &PlaylistItem, reasons: &mut ResolveReasonSet) -> bool {
    let needs_info = !pli.has_details() && resolve_options.has_flag(ResolveOptionsFlags::Resolve);

    if needs_info {
        reasons.set(ResolveReason::Info);
    }
    needs_info
}

fn check_resolve_tmdb(
    resolve_options: &ResolveOptions,
    resolve_tmdb_enabled: bool,
    pli: &PlaylistItem,
    reasons: &mut ResolveReasonSet,
) {
    if resolve_tmdb_enabled && resolve_options.has_flag(ResolveOptionsFlags::TmdbMissing) {
        if let Some(StreamProperties::Video(video_stream_props)) = pli.header.additional_properties.as_ref() {
            let has_tmdb = video_stream_props.tmdb.is_some();
            let has_date = video_stream_props.details.as_ref().and_then(|d| d.release_date.as_ref()).is_some();
            if !has_tmdb || !has_date {
                if !has_tmdb {
                    reasons.set(ResolveReason::Tmdb);
                }
                if !has_date {
                    reasons.set(ResolveReason::Date);
                }
            }
        }
    }
}

fn queue_background_vod_info(
    ctx: &PlaylistProcessingContext,
    fpl: &mut FetchedPlaylist<'_>,
    filter: impl Fn(&PlaylistItem) -> bool,
    resolve_options: &ResolveOptions,
    do_probe: bool,
    resolve_tmdb_enabled: bool,
) {
    let Some(mgr) = ctx.metadata_manager.as_ref() else {
        return;
    };

    let input = fpl.input;
    for pli in fpl.items_mut() {
        if !filter(pli) {
            continue;
        }

        let provider_id = if let Ok(uid) = pli.header.id.parse::<u32>() {
            ProviderIdType::Id(uid)
        } else {
            ProviderIdType::from(&*pli.header.id)
        };

        let reasons = check_resolve_reasons(resolve_options, do_probe, resolve_tmdb_enabled, pli);

        if !reasons.is_empty() {
            let task =
                UpdateTask::ResolveVod { id: provider_id, reason: reasons, delay: resolve_options.resolve_delay };
            mgr.queue_task_background(input.name.clone(), task);
        }
    }
}

async fn update_vod_info_immediate(
    ctx: &PlaylistProcessingContext,
    active_provider: &Arc<ActiveProviderManager>,
    input: &ConfigInput,
    pli: &PlaylistItem,
    id: ProviderIdType,
    reasons: &ResolveReasonSet,
    db_query: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>>,
) -> Result<Option<VideoStreamProperties>, TuliproxError> {
    let fetch_info = reasons.contains(ResolveReason::Info);
    let resolve_tmdb =
        fetch_info || reasons.contains(ResolveReason::Tmdb) || reasons.contains(ResolveReason::Date);

    update_vod_metadata(
        &ctx.config,
        &ctx.client,
        input,
        id,
        None, // No active handle, function will acquire one if needed
        active_provider,
        Some(&pli.header.title),
        false,
        fetch_info,
        resolve_tmdb,
        reasons.contains(ResolveReason::Probe),
        db_query,
    )
    .await
}

/// Updates metadata for a single VOD item (Info + Probe).
///
/// # Arguments
/// * `save` - If true, persists changes to the input database immediately (Instant strategy).
///   If false, returns the properties so the caller can batch persist them (Bundled strategy).
/// * `fetch_info` - If true, fetches details from Provider API. If false, uses existing/dummy data.
/// * `resolve_tmdb` - If true, resolves missing TMDB/date metadata from available titles.
/// * `db_query` - Optional pre-opened DB handle to avoid re-opening file.
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::fn_params_excessive_bools)]
pub async fn update_vod_metadata(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    id: ProviderIdType,
    active_handle: Option<&ProviderHandle>,
    active_provider: &Arc<ActiveProviderManager>,
    playlist_title: Option<&str>,
    save: bool,
    fetch_info: bool,
    resolve_tmdb: bool,
    do_probe: bool,
    db_query: Option<Arc<Mutex<BPlusTreeQuery<u32, XtreamPlaylistItem>>>>,
) -> Result<Option<VideoStreamProperties>, TuliproxError> {
    let working_dir = &app_config.config.load().working_dir;
    let storage_path = get_input_storage_path(&input.name, working_dir)
        .await
        .map_err(|e| shared::error::info_err!("Storage path error: {e}"))?;

    // Check if we should skip based on input options
    if input.has_flag(ConfigInputFlags::XtreamSkipVod) {
        return Ok(None);
    }

    // Try to load existing info first to check if we have title/o_name
    let mut props: Option<VideoStreamProperties> = None;
    let mut existing_item: Option<XtreamPlaylistItem> = None;

    let stream_id_opt = if let ProviderIdType::Id(vid) = id { Some(vid) } else { None };

    if let Some(stream_id) = stream_id_opt {
        if let Some(query) = db_query {
            let query = Arc::clone(&query);
            let item = match tokio::task::spawn_blocking(move || {
                let mut guard = query.lock();
                guard.query_zero_copy(&stream_id).ok().flatten()
            })
            .await
            {
                Ok(item) => item,
                Err(err) => {
                    error!("Failed to query VOD metadata from disk for {stream_id}: {err}");
                    None
                }
            };

            if let Some(item) = item {
                existing_item = Some(item.clone());
                if let Some(StreamProperties::Video(p)) = item.additional_properties.as_ref() {
                    props = Some(*p.clone());
                }
            }
        } else {
            let xtream_path = xtream_get_file_path(&storage_path, XtreamCluster::Video);
            if xtream_path.exists() {
                let _file_lock = app_config.file_locks.read_lock(&xtream_path).await;
                let xtream_path = xtream_path.clone();
                let item = match tokio::task::spawn_blocking(move || {
                    let mut query = BPlusTreeQuery::<u32, XtreamPlaylistItem>::try_new(&xtream_path)?;
                    query.query_zero_copy(&stream_id)
                })
                .await
                {
                    Ok(Ok(item)) => item,
                    Ok(Err(err)) => {
                        error!("Failed to query VOD metadata from disk for {stream_id}: {err}");
                        None
                    }
                    Err(err) => {
                        error!("Failed to query VOD metadata from disk for {stream_id}: {err}");
                        None
                    }
                };

                if let Some(item) = item {
                    existing_item = Some(item.clone());
                    if let Some(StreamProperties::Video(p)) = item.additional_properties.as_ref() {
                        props = Some(*p.clone());
                    }
                }
            }
        }
    }

    let mut fetched_new = false;
    let mut properties_updated = false;

    // Determine the title to use for logging and fallback.
    // Cloning here to satisfy borrow checker when 'props' is mutated later.
    let display_title = playlist_title
        .or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()))
        .or_else(|| props.as_ref().map(|p| p.name.as_ref()))
        .unwrap_or("Unknown")
        .to_string();

    let display_id = stream_id_opt.map_or_else(|| "StringID".to_string(), |v: u32| v.to_string());

    // Input DB is canonical, but if details are missing we still need one info fetch to complete the record.
    let should_fetch_info = fetch_info && props.as_ref().is_none_or(|p| p.details.is_none());

    // 1. Fetch Info from Provider (only when missing in input DB)
    if should_fetch_info {
        if let Some(stream_id) = stream_id_opt {
            let info_url = xtream::get_xtream_player_api_info_url(input, XtreamCluster::Video, stream_id)
                .ok_or_else(|| shared::error::info_err!("Failed to build info URL"))?;

            let input_source = InputSource::from(input).with_url(info_url);

            match xtream::get_xtream_stream_info_content(app_config, client, &input_source, true).await {
                Ok(content) => {
                    if !content.is_empty() {
                        if let Ok(mut json_value) = serde_json::from_str::<Value>(&content) {
                            if let Some(info) = json_value.get_mut("info").and_then(|v| v.as_object_mut()) {
                                crate::model::normalize_release_date(info);
                            }

                            if let Ok(info) = serde_json::from_value::<XtreamVideoInfo>(json_value) {
                                props = Some(if let Some(existing) = &existing_item {
                                    VideoStreamProperties::from_info(&info, existing)
                                } else {
                                    VideoStreamProperties::from_info_without_existing(&info)
                                });
                                fetched_new = true;
                                properties_updated = true;
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("Failed to fetch VOD info for {display_title} ({display_id}): {e}");
                }
            }
        }
    }

    // If no props yet, create dummy ones if we have enough info (at least a name/title)
    if props.is_none() {
        if let Some(title) = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref())) {
            let mut new_props = VideoStreamProperties {
                name: title.into(),
                stream_id: stream_id_opt.unwrap_or(0),
                container_extension: "".into(), // Will be filled later or by probe
                ..Default::default()
            };
            // Ensure details struct exists
            new_props.details = Some(VideoStreamDetailProperties::default());
            props = Some(new_props);
        } else {
            // We can't proceed without at least a name
            return Err(shared::error::info_err!("No VOD properties available and no title found for {display_id}"));
        }
    }

    let Some(mut properties) = props else {
        return Err(shared::error::info_err!("No VOD properties available after fallback creation for {display_id}"));
    };

    let resolve_tmdb_enabled = input.has_flag(ConfigInputFlags::ResolveTmdb);

    // 2. Resolve TMDB/Date if missing and explicitly enabled for this input
    let missing_tmdb = properties.tmdb.is_none();
    let missing_date = properties.details.as_ref().and_then(|d| d.release_date.as_ref()).is_none();

    if resolve_tmdb && resolve_tmdb_enabled && (missing_tmdb || missing_date) {
        // Try local parsing first
        if missing_date && !properties.name.is_empty() {
            let meta_parse = ptt_parse_title(&properties.name);
            if let Some(year) = meta_parse.year {
                if properties.details.is_none() {
                    properties.details = Some(VideoStreamDetailProperties::default());
                }
                if let Some(details) = properties.details.as_mut() {
                    details.release_date = Some(format!("{year}-01-01").into());
                    properties_updated = true;
                    debug_if_enabled!("Parsed local year for '{}': {}", properties.name, year);
                }
            }
        }

        // Re-check missing date after local parse
        let still_missing_date = properties.details.as_ref().and_then(|d| d.release_date.as_ref()).is_none();

        if missing_tmdb || still_missing_date {
            let config = app_config.config.load();
            let library_config = config.library.as_ref();
            let meta_resolver = MetadataResolver::new(library_config, client.clone());

            let mut meta = None;
            let mut tried_title = false;

            // 1. & 2. Playlist Title
            let title_candidate = playlist_title.or_else(|| existing_item.as_ref().map(|i| i.title.as_ref()));
            if let Some(title) = title_candidate {
                if !title.is_empty() {
                    debug!("Resolving TMDB for VOD using Playlist Title '{title}' (ID: {display_id})...");
                    meta = meta_resolver.resolve_from_title(title, properties.tmdb, true, resolve_tmdb_enabled).await;
                    tried_title = true;
                }
            }

            // 3. API Name (fallback)
            if (meta.is_none() || (meta.as_ref().is_some_and(|m| m.tmdb_id().is_none()))) && !properties.name.is_empty()
            {
                let title_already_tried =
                    if let Some(t) = title_candidate { t == properties.name.as_ref() } else { false };

                if !tried_title || !title_already_tried {
                    trace!("Fallback to API Name '{}'...", properties.name);
                    meta = meta_resolver
                        .resolve_from_title(&properties.name, properties.tmdb, true, resolve_tmdb_enabled)
                        .await;
                }
            }

            // 4. API Original Name (fallback)
            if meta.is_none() || (meta.as_ref().is_some_and(|m| m.tmdb_id().is_none())) {
                if let Some(o_name) = properties.details.as_ref().and_then(|d| d.o_name.as_deref()) {
                    if !o_name.is_empty() && o_name != properties.name.as_ref() {
                        trace!("Fallback to API Original Name '{o_name}'...");
                        meta =
                            meta_resolver.resolve_from_title(o_name, properties.tmdb, true, resolve_tmdb_enabled).await;
                    }
                }
            }

            if let Some(m) = meta {
                if properties.tmdb.is_none() {
                    properties.tmdb = m.tmdb_id();
                    properties_updated = true;
                }
                if let Some(details) = properties.details.as_mut() {
                    if details.release_date.is_none() {
                        details.release_date = m.year().map(|y| format!("{y}-01-01").into());
                        properties_updated = true;
                    }
                }
                if properties_updated {
                    let id_display = properties.tmdb.map_or("None".to_string(), |id| id.to_string());
                    trace_if_enabled!("Resolved TMDB for '{}' (ID: {}): {}", display_title, display_id, id_display);
                }
            }
        }
    }

    // 3. Probe (if enabled globally in config)
    let ffprobe_enabled = app_config.is_ffprobe_enabled().await;
    if do_probe && ffprobe_enabled {
        // Ensure details struct exists before probing
        if properties.details.is_none() {
            properties.details = Some(VideoStreamDetailProperties::default());
        }

        let (missing_video, missing_audio) = match properties.details.as_ref() {
            Some(details) => (
                !MediaQuality::is_valid_media_info(details.video.as_deref()),
                !MediaQuality::is_valid_media_info(details.audio.as_deref()),
            ),
            None => (true, true),
        };

        if missing_video || missing_audio {
            let input_url = input.url.as_str();
            let username = input.username.as_deref().unwrap_or("");
            let password = input.password.as_deref().unwrap_or("");
            let stream_url = crate::processing::parser::xtream::create_xtream_url(
                XtreamCluster::Video,
                input_url,
                username,
                password,
                &StreamProperties::Video(Box::new(properties.clone())),
                true,
                true,
            );

            let config = app_config.config.load();
            let metadata_update = config.metadata_update.clone().unwrap_or_default();
            let ffprobe_timeout = metadata_update.ffprobe_timeout.unwrap_or(60);
            let user_agent = config.default_user_agent.clone();
            let analyze_duration = metadata_update.ffprobe_analyze_duration_micros;
            let probe_size = metadata_update.ffprobe_probe_size_bytes;

            // Acquire Connection logic
            let temp_handle = if active_handle.is_some() {
                None // No new handle needed
            } else {
                active_provider.acquire_connection_for_probe(&input.name).await
            };

            if active_handle.is_some() || temp_handle.is_some() {
                debug_if_enabled!("Probing VOD '{}' (ID: {})", display_title, display_id);
                if let Some((_quality, raw_video, raw_audio)) = crate::utils::ffmpeg::probe_url(
                    &stream_url,
                    user_agent.as_deref(),
                    analyze_duration,
                    probe_size,
                    ffprobe_timeout,
                    config.proxy.as_ref(),
                )
                .await
                {
                    if let Some(details) = properties.details.as_mut() {
                        if let Some(v) = raw_video {
                            details.video = Some(v.to_string().into());
                            properties_updated = true;
                        }
                        if let Some(a) = raw_audio {
                            details.audio = Some(a.to_string().into());
                            properties_updated = true;
                        }
                    }
                }
                if let Some(h) = temp_handle {
                    active_provider.release_handle(&h).await;
                }
            } else {
                warn!("Skipping probe for VOD {display_id} due to connection limits");
            }
        }
    }

    // 4. Persist if updated
    if properties_updated || fetched_new {
        if save {
            if let Some(stream_id) = stream_id_opt {
                persist_input_vod_info(
                    app_config,
                    &storage_path,
                    XtreamCluster::Video,
                    &input.name,
                    stream_id,
                    &properties,
                )
                .await
                .map_err(|e| shared::error::info_err!("Persist error: {e}"))?;
            }

            debug_if_enabled!("Successfully updated VOD metadata for '{}' (ID: {})", display_title, display_id);
        }
        return Ok(Some(properties));
    }

    Ok(None)
}
