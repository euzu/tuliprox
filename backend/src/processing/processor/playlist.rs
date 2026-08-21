use crate::{
    api::{
        model::{
            ActiveProviderManager, AppState, EventManager, EventMessage, MetadataUpdateManager, PlaylistStorageState,
            ProviderIdType, ResolveReason, UpdateGuard, UpdateTask,
        },
        sync_panel_api_exp_dates,
    },
    iptv::{m3u, xtream},
    messaging::send_message,
    media_server::{
        media_server_catalog_snapshot_to_playlist, refresh_media_server_catalog_complete_before_publish,
        MediaServerCatalogRefreshPolicy, MediaServerHttpClient,
    },
    media_server::plex::client::PlexCatalogClient,
    model::{
        AppConfig, ConfigFavourites, ConfigInput, ConfigInputFlags, ConfigInputOptions, ConfigRename, ConfigTarget,
        Epg, FetchedPlaylist, Mapping, MessageContent, ProcessTargets, ReverseProxyDisabledHeaderConfig, TVGuide,
    },
    processing::{
        input_cache,
        input_cache::ClusterState,
        parser::xmltv::{flatten_tvguide, EpgMergeAccumulator, merge_epg_trees},
        playlist_watch::process_group_watch,
        processor::{
            epg::process_playlist_epg, library, sort::sort_playlist, stalker, StalkerRefreshMode,
            trakt::process_trakt_categories_for_target,
            xtream_series::playlist_resolve_series, xtream_vod::playlist_resolve_vod,
        },
    },
    repository::{
        load_input_playlist, persist_input_playlist, persist_playlist, CategoryKey, MemoryPlaylistSource,
        PlaylistSource,
    },
    utils::{
        debug_if_enabled, epg, log_memory_snapshot, trace_if_enabled,
        StepMeasure, StepMeasureCallback,
    },
};
use futures::{FutureExt, StreamExt};
use indexmap::IndexMap;
use log::{debug, error, info, log_enabled, warn, Level};
use path_clean::PathClean;
use shared::{
    concat_string,
    error::{get_errors_notify_message, TuliproxError},
    foundation::{get_field_value, set_field_value, Filter, ValueAccessor, ValueProvider},
    model::{
        ClusterFlags, CounterModifier, FieldGetAccessor, FieldSetAccessor, InputStats, InputType, ItemField,
        MappingStage, PlaylistGroup, PlaylistItem, PlaylistItemType, PlaylistStats, ProcessingOrder, SourceStats,
        StreamProperties, TargetStats, UUIDType, XtreamCluster,
    },
    utils::{
        create_alias_uuid, interner_gc,
        Internable,
    },
    defaults::{
        default_as_default, default_probe_delay_secs, default_probe_live_interval,
    }
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{watch, Mutex, OwnedRwLockWriteGuard, RwLock},
    task::JoinSet,
};
use shared::model::PlaylistUpdateProgressEvent;

const PLAYLIST_UPDATE_MAX_DURATION_SECS: u64 = 3600;
const MAX_CONCURRENT_TARGET_FINALIZERS: usize = 2;

fn join_arc_strs(values: &[Arc<str>], separator: &str) -> String {
    let mut result = String::new();
    for value in values {
        if !result.is_empty() {
            result.push_str(separator);
        }
        result.push_str(value.as_ref());
    }
    result
}

fn target_waiting_message(target: &str, input: &str) -> String {
    format!("Target '{target}' is waiting for input '{input}'")
}

fn target_mutated_resources(config: &crate::model::Config, target: &ConfigTarget) -> HashSet<PathBuf> {
    let mut resources = HashSet::new();
    if let Some(path) = crate::repository::get_target_storage_path(config, &target.name) {
        resources.insert(path.clean());
    }
    for output in &target.output {
        match output {
            crate::model::TargetOutput::M3u(output) => {
                if let Some(path) = crate::utils::get_file_path(
                    &config.storage_dir,
                    output.filename.as_deref().map(PathBuf::from),
                ) {
                    resources.insert(path.clean());
                }
            }
            crate::model::TargetOutput::Strm(output) => {
                if let Some(path) =
                    crate::utils::get_file_path(&config.storage_dir, Some(PathBuf::from(&output.directory)))
                {
                    resources.insert(path.clean());
                }
            }
            crate::model::TargetOutput::Xtream(_) | crate::model::TargetOutput::HdHomeRun(_) => {}
        }
    }
    resources
}

fn stalker_checkpoint_message(input: &str) -> String {
    format!("Input '{input}': Stalker refresh checkpoint saved; active snapshot remains in service")
}

fn is_valid(pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool) -> bool {
    let provider = ValueProvider { pli, match_as_ascii };
    filter.filter(&provider)
}

pub fn apply_filter_to_source(source: &mut PlaylistSource, filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    let mut groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
    for pli in source.into_items() {
        if is_valid(&pli, filter, false) {
            let group_title = pli.header.group.clone();
            let cluster = pli.header.xtream_cluster;
            let cat_id = pli.header.category_id;
            let normalized_group = shared::utils::deunicode_string(&group_title).to_lowercase().intern();
            let key = (cluster, normalized_group);
            groups
                .entry(key)
                .or_insert_with(|| PlaylistGroup {
                    id: cat_id,
                    title: group_title,
                    channels: vec![],
                    xtream_cluster: cluster,
                })
                .channels
                .push(pli);
        }
    }

    if groups.is_empty() {
        None
    } else {
        Some(groups.into_values().collect())
    }
}

fn filter_playlist(source: &mut PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    apply_filter_to_source(source, &target.filter)
}

pub fn apply_filter_to_playlist(playlist: &mut [PlaylistGroup], filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    // NOTE: the source `playlist` is intentionally cloned (not drained) here because
    // the caller reuses the same slice for every target output and for the no-filter
    // fallback path, so the survivors cannot be moved out of it. Cap the initial
    // allocation so selective filters do not retain capacity for every source item.
    const INITIAL_FILTERED_GROUP_CAPACITY: usize = 256;
    let mut new_playlist = Vec::with_capacity(playlist.len());
    for pg in playlist.iter() {
        let mut channels = Vec::with_capacity(pg.channels.len().min(INITIAL_FILTERED_GROUP_CAPACITY));
        channels.extend(pg.channels.iter().filter(|&pli| is_valid(pli, filter, false)).cloned());
        if !channels.is_empty() {
            new_playlist.push(PlaylistGroup {
                id: pg.id,
                title: pg.title.clone(),
                channels,
                xtream_cluster: pg.xtream_cluster,
            });
        }
    }
    if new_playlist.is_empty() {
        None
    } else {
        Some(new_playlist)
    }
}

fn assign_channel_no_playlist(new_playlist: &mut [PlaylistGroup]) {
    let assigned_chnos: HashSet<u32> =
        new_playlist.iter().flat_map(|g| &g.channels).filter(|c| c.header.chno != 0).map(|c| c.header.chno).collect();
    let mut chno = 1;
    for group in new_playlist {
        for chan in &mut group.channels {
            if chan.header.chno == 0 {
                while assigned_chnos.contains(&chno) {
                    chno += 1;
                }
                chan.header.chno = chno;
                chno += 1;
            }
        }
    }
}

fn exec_rename(pli: &mut PlaylistItem, rename: Option<&Vec<ConfigRename>>) {
    if let Some(renames) = rename {
        if !renames.is_empty() {
            let result = pli;
            for r in renames {
                let value = get_field_value(result, r.field);
                let cap = r.pattern.replace_all(&value, &r.new_name);
                if log_enabled!(log::Level::Debug) && *value != *cap {
                    trace_if_enabled!("Renamed {}={value} to {cap}", &r.field);
                }
                set_field_value(result, r.field, cap.as_ref());
            }
        }
    }
}

fn rename_playlist(source: &mut PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    match &target.rename {
        Some(renames) if !renames.is_empty() => {
            let mut groups: IndexMap<(XtreamCluster, Arc<str>), PlaylistGroup> = IndexMap::new();
            for mut pli in source.into_items() {
                // Handle group rename first if it's in the renames
                for r in renames {
                    if matches!(r.field, ItemField::Group) {
                        let value = &*pli.header.group;
                        let cap = r.pattern.replace_all(value, &r.new_name);
                        if *value != cap {
                            pli.header.group = cap.intern();
                        }
                    }
                }
                exec_rename(&mut pli, Some(renames));
                let group_title = pli.header.group.clone();
                let cluster = pli.header.xtream_cluster;
                let cat_id = pli.header.category_id;
                groups
                    .entry((cluster, group_title.clone()))
                    .or_insert_with(|| PlaylistGroup {
                        id: cat_id,
                        title: group_title,
                        channels: vec![],
                        xtream_cluster: cluster,
                    })
                    .channels
                    .push(pli);
            }
            Some(groups.into_values().collect())
        }
        _ => None,
    }
}

fn map_channel(mut channel: PlaylistItem, mapping: &Mapping) -> (PlaylistItem, Vec<PlaylistItem>, bool) {
    let mut matched = false;
    let mut virtual_items = vec![];
    if let Some(mapper) = &mapping.mapper {
        if !mapper.is_empty() {
            let ref_chan = &mut channel;
            let templates = mapping.templates.as_ref();
            for m in mapper {
                if let Some(script) = m.t_script.as_ref() {
                    if let Some(filter) = &m.t_filter {
                        let provider = ValueProvider { pli: ref_chan, match_as_ascii: mapping.match_as_ascii };
                        if filter.filter(&provider) {
                            matched = true;
                            let mut accessor = ValueAccessor {
                                pli: ref_chan,
                                virtual_items: vec![],
                                match_as_ascii: mapping.match_as_ascii,
                            };
                            script.eval(&mut accessor, templates.map(Vec::as_slice));
                            virtual_items.extend(accessor.virtual_items.into_iter().map(|(_, pli)| pli));
                        }
                    }
                }
            }
        }
    }
    (channel, virtual_items, matched)
}

fn map_channel_and_flatten(channel: PlaylistItem, mapping: &Mapping) -> Vec<PlaylistItem> {
    let (mapped_channel, mut virtual_items, _matched) = map_channel(channel, mapping);
    let mut result = Vec::with_capacity(1 + virtual_items.len());

    result.push(mapped_channel);
    result.append(&mut virtual_items);
    result
}

fn map_playlist(source: &mut PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    map_playlist_at_stage(source, target, MappingStage::Processing, None)
}

fn map_playlist_at_stage(
    source: &mut PlaylistSource,
    target: &ConfigTarget,
    stage: MappingStage,
    duplicates: Option<&mut HashSet<UUIDType>>,
) -> Option<Vec<PlaylistGroup>> {
    let mapping_binding = target.mapping.load();
    let mappings = mapping_binding.as_ref()?;
    let mut valid_mappings = mappings
        .iter()
        .filter(|m| m.stage == stage && m.mapper.as_ref().is_some_and(|items| !items.is_empty()))
        .peekable();
    valid_mappings.peek()?;
    let original_ids = if duplicates.is_some() {
        Some(source.items().map(|item| *item.header.get_uuid()).collect::<HashSet<_>>())
    } else {
        None
    };
    let iter: Box<dyn Iterator<Item=PlaylistItem>> = Box::new(source.into_items());
    let mapped_iter = valid_mappings.fold(iter, |iter, mapping| {
        Box::new(iter.flat_map(move |chan| map_channel_and_flatten(chan, mapping)))
            as Box<dyn Iterator<Item=PlaylistItem>>
    });
    let mut next_groups: IndexMap<CategoryKey, PlaylistGroup> = IndexMap::new();
    let mut grp_id: u32 = 0;
    for channel in mapped_iter {
        let group_title = channel.header.group.clone();
        let cluster = channel.header.xtream_cluster;
        next_groups
            .entry((cluster, group_title.clone()))
            .or_insert_with(|| {
                grp_id += 1;
                PlaylistGroup { id: grp_id, title: group_title, channels: Vec::new(), xtream_cluster: cluster }
            })
            .channels
            .push(channel);
    }

    let mut groups = next_groups.into_values().collect::<Vec<_>>();
    if let (Some(original_ids), Some(duplicates)) = (original_ids, duplicates) {
        for group in &mut groups {
            group.channels.retain(|item| {
                let uuid = *item.header.get_uuid();
                original_ids.contains(&uuid) || duplicates.insert(uuid)
            });
        }
    }
    Some(groups)
}

fn map_playlist_counter(target: &ConfigTarget, playlist: &mut [PlaylistGroup]) {
    if let Some(guard) = &*target.mapping.load() {
        let mappings = guard.as_ref();
        for mapping in mappings {
            if let Some(counter_list) = &mapping.t_counter {
                for counter in counter_list {
                    // fresh per target/call. No shared atomic, no cross-refresh carry-over.
                    let mut current = counter.start;
                    for plg in &mut *playlist {
                        for channel in &mut plg.channels {
                            let provider = ValueProvider { pli: channel, match_as_ascii: mapping.match_as_ascii };
                            if counter.filter.filter(&provider) {
                                let cntval = current;
                                current += 1;
                                let padded_cntval = if counter.padding > 0 {
                                    format!("{:0width$}", cntval, width = counter.padding as usize)
                                } else {
                                    cntval.to_string()
                                };
                                let new_value = if counter.modifier == CounterModifier::Assign {
                                    padded_cntval
                                } else {
                                    let value = channel
                                        .header
                                        .get_field(&counter.field)
                                        .map_or_else(String::new, |field_value| field_value.to_string());
                                    if counter.modifier == CounterModifier::Suffix {
                                        format!("{value}{}{padded_cntval}", counter.concat)
                                    } else {
                                        format!("{padded_cntval}{}{value}", counter.concat)
                                    }
                                };
                                channel.header.set_field(&counter.field, new_value.as_str());
                            }
                        }
                    }
                }
            }
        }
    }
}

// Inputs disabled in the config are always disabled.
// Command-line targets can only restrict enabled inputs, never enable them.
fn is_input_enabled(input: &ConfigInput, user_targets: &ProcessTargets) -> bool {
    input.enabled && (!user_targets.enabled || user_targets.has_input(input.id))
}

fn is_target_enabled(target: &ConfigTarget, user_targets: &ProcessTargets) -> bool {
    (!user_targets.enabled && target.enabled) || (user_targets.enabled && user_targets.has_target(target.id))
}

async fn with_sequential_group<T>(
    file_locks: &crate::utils::FileLockManager,
    group: Option<u32>,
    process_parallel: bool,
    future: impl std::future::Future<Output = T>,
) -> T {
    let _guard = if process_parallel {
        if let Some(group) = group {
            Some(file_locks.write_lock_str(&format!("sequential_group:{group}")).await)
        } else {
            None
        }
    } else {
        None
    };
    future.await
}

struct PlaylistDownloadResult {
    pub downloaded_playlist: Vec<PlaylistGroup>,
    pub download_err: Vec<TuliproxError>,
    pub was_cached: bool,
    pub persisted: bool,
    pub partial: bool,
}

impl PlaylistDownloadResult {
    pub fn new(
        downloaded_playlist: Vec<PlaylistGroup>,
        download_err: Vec<TuliproxError>,
        was_cached: bool,
        persisted: bool,
    ) -> Self {
        Self { downloaded_playlist, download_err, was_cached, persisted, partial: false }
    }

    fn with_partial(mut self, partial: bool) -> Self {
        self.partial = partial;
        self
    }
}

fn collect_effective_skip_clusters(input: &ConfigInput) -> Vec<XtreamCluster> {
    if !input.input_type.is_xtream() {
        return vec![];
    }
    xtream::get_skip_cluster(input)
}

async fn download_plex_media_server_playlist(
    client: &reqwest::Client,
    input: &ConfigInput,
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>) {
    let Some(media_server) = input.media_server.as_ref() else {
        return (
            vec![],
            vec![TuliproxError::Download(format!(
                "media-server input '{}' is missing media_server configuration",
                input.name
            ))],
        );
    };
    let http_client = MediaServerHttpClient::new(client.clone());
    let plex_client = match PlexCatalogClient::from_input(input, http_client) {
        Ok(client) => client,
        Err(error) => return (vec![], vec![TuliproxError::Download(error.to_string())]),
    };
    let policy = MediaServerCatalogRefreshPolicy {
        page_size: usize::from(media_server.catalog.page_size),
        request_delay_ms: media_server.catalog.request_delay_ms,
    };

    match refresh_media_server_catalog_complete_before_publish(&plex_client, policy).await {
        Ok(snapshot) => (media_server_catalog_snapshot_to_playlist(&snapshot), vec![]),
        Err(error) => (vec![], vec![TuliproxError::Download(error.to_string())]),
    }
}

fn filter_skipped_clusters_from_source(source: PlaylistSource, input: &ConfigInput) -> PlaylistSource {
    let skip_clusters = collect_effective_skip_clusters(input);
    if skip_clusters.is_empty() {
        return source;
    }

    let skip_set: HashSet<XtreamCluster> = skip_clusters.into_iter().collect();
    PlaylistSource::filtered(source, skip_set)
}

fn cluster_selected(cluster: XtreamCluster, clusters: ClusterFlags) -> bool {
    match cluster {
        XtreamCluster::Live => clusters.contains(ClusterFlags::Live),
        XtreamCluster::Video => clusters.contains(ClusterFlags::Vod),
        XtreamCluster::Series => clusters.contains(ClusterFlags::Series),
    }
}

fn apply_staged_overlay_groups(
    provider_name: &Arc<str>,
    clusters: ClusterFlags,
    provider_groups: Vec<PlaylistGroup>,
    staged_groups: Vec<PlaylistGroup>,
) -> Vec<PlaylistGroup> {
    let mut groups: Vec<PlaylistGroup> = provider_groups
        .into_iter()
        .filter(|group| !cluster_selected(group.xtream_cluster, clusters))
        .collect();

    groups.extend(
        staged_groups
            .into_iter()
            .filter(|group| cluster_selected(group.xtream_cluster, clusters))
            .map(|mut group| {
                for item in &mut group.channels {
                    item.header.input_name = Arc::clone(provider_name);
                }
                group
            }),
    );

    groups
}

fn should_apply_staged_overlay(download_result: &PlaylistDownloadResult) -> bool {
    !download_result.was_cached
}

#[allow(clippy::too_many_lines)]
async fn playlist_download_from_input(
    client: &reqwest::Client,
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    stalker_refresh_mode: StalkerRefreshMode,
) -> PlaylistDownloadResult {
    let config = &*app_config.config.load();
    let storage_dir = &config.storage_dir;

    // Check Status
    let storage_path = input_cache::resolve_input_storage_path(storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&storage_path);
    let cache_duration = input.cache_duration_seconds;

    // Ensure data directory exists
    match tokio::fs::try_exists(&storage_path).await {
        Ok(false) => {
            if let Err(err) = tokio::fs::create_dir_all(&storage_path).await {
                warn!("Failed to create input storage directory '{}': {err}", storage_path.display());
            }
        }
        Err(err) => {
            warn!("Failed to check existence of input storage directory '{}': {err}", storage_path.display());
        }
        Ok(true) => {}
    }

    let download_input_type = input.get_download_input_type();
    // Use per-cluster cache for effective Xtream downloads.
    let use_per_cluster_cache = download_input_type.is_xtream();

    let mut xtream_clusters_to_download = Vec::new();
    let fully_cached = if use_per_cluster_cache {
        let skip_cluster = collect_effective_skip_clusters(input);
        let xtream_cache_candidates = xtream::requested_clusters(None, &skip_cluster);

        for cluster in xtream_cache_candidates {
            if !input_cache::is_cache_valid(&status, cluster.as_ref(), cache_duration) {
                xtream_clusters_to_download.push(cluster);
            }
        }

        xtream_clusters_to_download.is_empty()
    } else {
        input_cache::is_cache_valid(&status, "default", cache_duration)
    };

    if fully_cached {
        return PlaylistDownloadResult::new(vec![], vec![], true, false);
    }

    let (playlist, errors, persisted, _m3u_error_count, _xtream_error_count, partial) = {
        match download_input_type {
            InputType::M3u => {
                let (p, e) = m3u::download_m3u_playlist(app_config, client, config, input).await;
                (p, e, false, 0, 0, false)
            }
            InputType::Xtream => {
                let (p, e, persisted) = xtream::download_xtream_playlist(
                    app_config,
                    client,
                    input,
                    Some(xtream_clusters_to_download.as_slice()),
                )
                    .await;
                let xtream_error_count = e.len();
                (p, e, persisted, 0, xtream_error_count, false)
            }
            InputType::M3uBatch | InputType::XtreamBatch | InputType::StalkerBatch => {
                (vec![], vec![], false, 0, 0, false)
            }
            InputType::Stalker => {
                let (p, e, persisted, partial) = stalker::download_stalker_playlist(
                    app_config,
                    client,
                    input,
                    None,
                    stalker_refresh_mode,
                    !config.disk_based_processing,
                )
                .await;
                let stalker_error_count = e.len();
                (p, e, persisted, 0, stalker_error_count, partial)
            }
            InputType::Library => {
                let (p, e) = library::download_library_playlist(client, app_config, input).await;
                (p, e, false, 0, 0, false)
            }
            InputType::Plex => {
                let (p, e) = download_plex_media_server_playlist(client, input).await;
                (p, e, false, 0, 0, false)
            }
            InputType::Emby | InputType::Jellyfin => (
                vec![],
                vec![TuliproxError::Download(format!(
                    "media-server input '{}' is configured but catalog import is not implemented yet",
                    input.name
                ))],
                false,
                0,
                0,
                false,
            ),
            InputType::Staged => (
                vec![],
                vec![TuliproxError::Download(format!(
                    "staged input '{}' was not resolved against a parent input",
                    input.name
                ))],
                false,
                0,
                0,
                false,
            ),
        }
    };

    // Update Status
    let save_status;
    if partial {
        input_cache::update_cluster_status(&mut status, "default", ClusterState::Failed);
        save_status = true;
    } else if errors.is_empty() {
        if use_per_cluster_cache {
            for cluster in &xtream_clusters_to_download {
                input_cache::update_cluster_status(&mut status, cluster.as_ref(), ClusterState::Ok);
            }
            save_status = !xtream_clusters_to_download.is_empty();
        } else {
            input_cache::update_cluster_status(&mut status, "default", ClusterState::Ok);
            save_status = true;
        }
    } else if use_per_cluster_cache {
        for cluster in &xtream_clusters_to_download {
            input_cache::update_cluster_status(&mut status, cluster.as_ref(), ClusterState::Failed);
        }
        save_status = !xtream_clusters_to_download.is_empty();
    } else {
        input_cache::update_cluster_status(&mut status, "default", ClusterState::Failed);
        save_status = true;
    }

    if save_status {
        input_cache::save_input_status(&storage_path, &status);
    }

    PlaylistDownloadResult::new(playlist, errors, false, persisted).with_partial(partial)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InputJobState {
    Ready,
    Pending,
    Failed,
}

struct InputJobResult {
    index: usize,
    input_name: Arc<str>,
    state: InputJobState,
    source: Option<PlaylistSource>,
    epg: Option<TVGuide>,
    stat: InputStats,
    errors: Vec<TuliproxError>,
}

async fn process_input_job(
    index: usize,
    ctx: &PlaylistProcessingContext,
    input: &Arc<ConfigInput>,
    process_parallel: bool,
) -> InputJobResult {
    with_sequential_group(
        &ctx.config.file_locks,
        input.sequential_group,
        process_parallel,
        process_input_job_inner(index, ctx, input),
    )
    .await
}

async fn process_input_job_inner(
    index: usize,
    ctx: &PlaylistProcessingContext,
    input: &Arc<ConfigInput>,
) -> InputJobResult {
    let start_time = Instant::now();
    let input_type = input.get_download_input_type();
    let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());
    broadcast_step("Playlist download", &format!("Downloading input '{}'", input.name));

    let (mut errors, mut source, storage_error, partial) = download_input(ctx, input, false).await;
    let storage_failed = storage_error.is_some();
    if let Some(err) = storage_error {
        broadcast_step(
            "Playlist download",
            &format!("Failed to persist/load input '{}' playlist", input.name),
        );
        error!("Failed to persist input playlist {}", input.name);
        errors.push(err);
    }
    let epg = if input_type == InputType::Library || partial || storage_failed {
        None
    } else {
        download_input_epg(ctx, input, &mut errors).await
    };
    let group_count = source.get_group_count();
    let channel_count = source.get_channel_count();
    let state = if partial {
        InputJobState::Pending
    } else if storage_failed || source.is_empty() {
        if source.is_empty() {
            broadcast_step("Playlist download", &format!("Input '{}' playlist is empty", input.name));
            errors.push(TuliproxError::RepositoryPlaylist(format!("Source is empty {}", input.name)));
        }
        InputJobState::Failed
    } else {
        InputJobState::Ready
    };
    let stat = create_input_stat(
        group_count,
        channel_count,
        errors.len(),
        input_type,
        &input.name,
        start_time.elapsed().as_secs(),
    );

    InputJobResult {
        index,
        input_name: input.name.clone(),
        state,
        source: (state == InputJobState::Ready).then_some(source),
        epg,
        stat,
        errors,
    }
}

fn panicked_input_job(index: usize, input: &ConfigInput) -> InputJobResult {
    let error = TuliproxError::RepositoryPlaylist(format!("Input '{}' processing panicked", input.name));
    InputJobResult {
        index,
        input_name: input.name.clone(),
        state: InputJobState::Failed,
        source: None,
        epg: None,
        stat: create_input_stat(0, 0, 1, input.get_download_input_type(), &input.name, 0),
        errors: vec![error],
    }
}

struct TargetJobResult {
    index: usize,
    name: String,
    result: Result<(), Vec<TuliproxError>>,
    errors: Vec<TuliproxError>,
}

fn collect_target_task_result(
    result: Result<TargetJobResult, tokio::task::JoinError>,
    results: &mut Vec<TargetJobResult>,
    errors: &mut Vec<TuliproxError>,
) {
    match result {
        Ok(result) => results.push(result),
        Err(err) => errors.push(TuliproxError::RepositoryPlaylist(format!(
            "Target finalization task failed: {err}"
        ))),
    }
}

async fn wait_for_target_finalizer_slot(
    tasks: &mut JoinSet<TargetJobResult>,
    results: &mut Vec<TargetJobResult>,
    errors: &mut Vec<TuliproxError>,
) {
    if tasks.len() >= MAX_CONCURRENT_TARGET_FINALIZERS {
        if let Some(result) = tasks.join_next().await {
            collect_target_task_result(result, results, errors);
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn process_targets(
    ctx: &Arc<PlaylistProcessingContext>,
    playlists: &mut [FetchedPlaylist<'_>],
    targets: &[&Arc<ConfigTarget>],
    input_stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    process_parallel: bool,
) -> Vec<TargetStats> {
    if !process_parallel {
        let mut target_stats = Vec::with_capacity(targets.len());
        for (index, target) in targets.iter().enumerate() {
            let consume_input_source = index + 1 == targets.len();
            let result =
                prepare_playlist_for_target(ctx, playlists, target, input_stats, errors, consume_input_source).await;
            match result {
                Ok(prepared) => {
                    let (result, mut finalization_errors) =
                        finalize_prepared_target(Arc::clone(ctx), prepared).await;
                    errors.append(&mut finalization_errors);
                    match result {
                        Ok(()) => target_stats.push(TargetStats::success(&target.name)),
                        Err(mut target_errors) => {
                            target_stats.push(TargetStats::failure(&target.name));
                            errors.append(&mut target_errors);
                        }
                    }
                }
                Err(mut target_errors) => {
                    target_stats.push(TargetStats::failure(&target.name));
                    errors.append(&mut target_errors);
                }
            }
        }
        return target_stats;
    }

    let resources = {
        let config = ctx.config.config.load();
        targets.iter().map(|target| target_mutated_resources(&config, target)).collect::<Vec<_>>()
    };
    let mut completion_receivers: Vec<watch::Receiver<bool>> = Vec::with_capacity(targets.len());
    let mut tasks = JoinSet::new();
    let mut results = Vec::with_capacity(targets.len());

    for (index, target) in targets.iter().enumerate() {
        wait_for_target_finalizer_slot(&mut tasks, &mut results, errors).await;
        let predecessors = resources[..index]
            .iter()
            .zip(&completion_receivers)
            .filter(|(earlier, _)| !earlier.is_disjoint(&resources[index]))
            .map(|(_, receiver)| receiver.clone())
            .collect::<Vec<_>>();
        let (completion, receiver) = watch::channel(false);
        completion_receivers.push(receiver);

        match prepare_playlist_for_target(ctx, playlists, target, input_stats, errors, false).await {
            Ok(prepared) => {
                let task_ctx = Arc::clone(ctx);
                let target_name = target.name.clone();
                tasks.spawn(async move {
                    for mut predecessor in predecessors {
                        if !*predecessor.borrow() {
                            let _ = predecessor.changed().await;
                        }
                    }
                    let finalized =
                        std::panic::AssertUnwindSafe(finalize_prepared_target(task_ctx, prepared)).catch_unwind().await;
                    completion.send_replace(true);
                    match finalized {
                        Ok((result, errors)) => TargetJobResult { index, name: target_name, result, errors },
                        Err(_) => TargetJobResult {
                            index,
                            name: target_name.clone(),
                            result: Err(vec![TuliproxError::RepositoryPlaylist(format!(
                                "Target '{target_name}' finalization panicked"
                            ))]),
                            errors: Vec::new(),
                        },
                    }
                });
            }
            Err(target_errors) => {
                completion.send_replace(true);
                results.push(TargetJobResult {
                    index,
                    name: target.name.clone(),
                    result: Err(target_errors),
                    errors: Vec::new(),
                });
            }
        }
    }

    while let Some(result) = tasks.join_next().await {
        collect_target_task_result(result, &mut results, errors);
    }
    results.sort_by_key(|result| result.index);

    let mut target_stats = Vec::with_capacity(results.len());
    for mut target_result in results {
        errors.append(&mut target_result.errors);
        match target_result.result {
            Ok(()) => target_stats.push(TargetStats::success(&target_result.name)),
            Err(mut target_errors) => {
                target_stats.push(TargetStats::failure(&target_result.name));
                errors.append(&mut target_errors);
            }
        }
    }
    target_stats
}

#[allow(clippy::too_many_lines)]
async fn process_source(
    source_idx: usize,
    ctx: Arc<PlaylistProcessingContext>,
) -> (Vec<InputStats>, Vec<TargetStats>, Vec<TuliproxError>) {
    log_memory_snapshot(format!("source[{source_idx}] start").as_str());
    let sources = ctx.config.sources.load();
    let mut errors = vec![];
    let mut input_stats = HashMap::<Arc<str>, InputStats>::new();
    let mut target_stats = Vec::<TargetStats>::new();
    if let Some(source) = sources.get_source_at(source_idx) {
        let mut source_playlists = Vec::with_capacity(source.inputs.len());
        let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());
        let process_parallel = ctx.config.config.load().process_parallel;
        let mut disabled_inputs: Vec<Arc<str>> = vec![];
        let mut enabled_inputs = Vec::with_capacity(source.inputs.len());
        for (index, input_name) in source.inputs.iter().enumerate() {
            let Some(input) = sources.get_input_by_name(input_name) else {
                error!("Input {input_name} referenced by source {source_idx} does not exist");
                continue;
            };
            if is_input_enabled(input, &ctx.user_targets) {
                enabled_inputs.push((index, input));
            } else {
                disabled_inputs.push(input.name.clone());
            }
        }

        let source_downloaded = !enabled_inputs.is_empty();
        let mut job_results = Vec::with_capacity(enabled_inputs.len());
        if process_parallel {
            let mut jobs = futures::stream::FuturesUnordered::new();
            for &(index, input) in &enabled_inputs {
                let job = std::panic::AssertUnwindSafe(process_input_job(index, &ctx, input, true)).catch_unwind();
                jobs.push(async move {
                    match job.await {
                        Ok(result) => result,
                        Err(_) => panicked_input_job(index, input),
                    }
                });
            }
            while let Some(result) = jobs.next().await {
                job_results.push(result);
            }
        } else {
            for &(index, input) in &enabled_inputs {
                job_results.push(process_input_job(index, &ctx, input, false).await);
            }
        }
        job_results.sort_by_key(|result| result.index);

        let mut blockers = Vec::new();
        for mut result in job_results {
            errors.append(&mut result.errors);
            input_stats.insert(result.input_name.clone(), result.stat);
            if result.state == InputJobState::Ready {
                if let (Some(input), Some(source)) = (
                    sources.get_input_by_name(&result.input_name),
                    result.source.take(),
                ) {
                    source_playlists.push(FetchedPlaylist { input, source, epg: result.epg });
                }
            } else {
                blockers.push(result.input_name);
            }
        }

        if !disabled_inputs.is_empty() && !source_downloaded {
            warn!(
                "Source at index {source_idx} has no enabled inputs for the given targets. Disabled: {}",
                join_arc_strs(&disabled_inputs, ", ")
            );
        }
        if source_downloaded {
            if !blockers.is_empty() {
                for target in source.targets.iter().filter(|target| is_target_enabled(target, &ctx.user_targets)) {
                    for input_name in &blockers {
                        broadcast_step(
                            "Playlist download",
                            &target_waiting_message(&target.name, input_name),
                        );
                    }
                }
            } else if source_playlists.is_empty() {
                debug!("Source at index {source_idx} is empty");
                errors.push(TuliproxError::RepositoryPlaylist(format!(
                    "Source at index {source_idx} is empty: {}",
                    join_arc_strs(&source.inputs, ", ")
                )));
            } else {
                debug_if_enabled!(
                    "Source has {} groups",
                    source_playlists.iter_mut().map(FetchedPlaylist::get_channel_count).sum::<usize>()
                );
                let enabled_targets: Vec<_> =
                    source.targets.iter().filter(|target| is_target_enabled(target, &ctx.user_targets)).collect();
                target_stats = process_targets(
                    &ctx,
                    &mut source_playlists,
                    &enabled_targets,
                    &mut input_stats,
                    &mut errors,
                    process_parallel,
                )
                .await;
            }
        }
    }
    log_memory_snapshot(format!("source[{source_idx}] end").as_str());
    let ordered_input_stats = sources.get_source_at(source_idx).map_or_else(Vec::new, |source| {
        source.inputs.iter().filter_map(|name| input_stats.remove(name)).collect()
    });
    (ordered_input_stats, target_stats, errors)
}

async fn download_input_epg(
    ctx: &PlaylistProcessingContext,
    input: &Arc<ConfigInput>,
    error_list: &mut Vec<TuliproxError>,
) -> Option<TVGuide> {
    // Download epg for input
    let (tvguide, mut tvguide_errors) = if error_list.is_empty() {
        let storage_dir = &ctx.config.config.load().storage_dir;
        epg::get_xmltv(ctx, input, None, storage_dir).await
    } else {
        (None, vec![])
    };
    error_list.append(&mut tvguide_errors);
    tvguide
}

/// `invalidate_input_cache_status` performs a non-atomic file I/O sequence
/// (`input_cache::load_input_status` + `input_cache::save_input_status`).
/// Call this only while holding the per-input lock from
/// `PlaylistProcessingContext::get_input_lock` (as done in `download_input`).
async fn invalidate_input_cache_status(ctx: &PlaylistProcessingContext, input: &ConfigInput) {
    let storage_dir = { ctx.config.config.load().storage_dir.clone() };
    let storage_path = input_cache::resolve_input_storage_path(&storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&storage_path);
    if !status.clusters.is_empty() {
        status.clusters.clear();
        input_cache::save_input_status(&storage_path, &status);
    }
}

async fn load_cached_input_playlist(
    ctx: &PlaylistProcessingContext,
    input: &Arc<ConfigInput>,
) -> (PlaylistSource, Option<TuliproxError>) {
    match load_input_playlist(ctx, input, None).await {
        Ok(pl_source) => (pl_source, None),
        Err(err) => (MemoryPlaylistSource::default().into_source(), Some(err)),
    }
}

#[allow(clippy::too_many_lines)]
async fn download_input(
    ctx: &PlaylistProcessingContext,
    input: &Arc<ConfigInput>,
    allow_staged_input: bool,
) -> (Vec<TuliproxError>, PlaylistSource, Option<TuliproxError>, bool) {
    if input.staged.is_some() && !allow_staged_input {
        return (vec![], MemoryPlaylistSource::default().into_source(), None, false);
    }

    let staged_overlay = if input.staged.is_none() {
        let sources = ctx.config.sources.load();
        sources.get_staged_input_for_provider(&input.name).cloned()
    } else {
        None
    };

    // Coordination Logic
    let need_download = !ctx.is_input_downloaded(&input.name).await;
    // Keep this lock for the whole critical section (download + persist/load + mark processed)
    // so parallel sources sharing the same input cannot observe a half-written state.
    let mut input_lock = if need_download { Some(ctx.get_input_lock(&input.name).await) } else { None };
    let mut mark_as_processed = false;

    let mut playlist_download_result = if need_download {
        // Check again after lock
        let already_processed = ctx.is_input_downloaded(&input.name).await;

        if already_processed {
            // Use empty results, will load from disk below
            PlaylistDownloadResult::new(vec![], vec![], true, false)
        } else if ctx.pre_processed_inputs.as_ref().is_some_and(|s| s.contains(&input.name)) {
            // Input was already processed in a prior session; skip download and load from disk.
            // Mark only after load succeeds (or fails) to avoid exposing a half-ready state.
            mark_as_processed = true;
            PlaylistDownloadResult::new(vec![], vec![], true, false)
        } else {
            mark_as_processed = true;
            playlist_download_from_input(&ctx.client, &ctx.config, input, ctx.stalker_refresh_mode).await
        }
    } else {
        PlaylistDownloadResult::new(vec![], vec![], true, false)
    };

    let mut preloaded_playlist: Option<(PlaylistSource, Option<TuliproxError>)> = None;
    if playlist_download_result.was_cached {
        let (cached_playlist, cached_error) = load_cached_input_playlist(ctx, input).await;
        // Defensive fallback: if cache metadata says "valid" but persisted data is unreadable,
        // retry once before forcing a refresh.
        let must_force_refresh = cached_error.is_some();
        if must_force_refresh {
            warn!(
                "Input '{}' cache hit produced unreadable playlist; retrying cached load once",
                input.name
            );
            let (retry_playlist, retry_error) = load_cached_input_playlist(ctx, input).await;
            if retry_error.is_none() {
                preloaded_playlist = Some((retry_playlist, None));
            } else {
                if input_lock.is_none() {
                    input_lock = Some(ctx.get_input_lock(&input.name).await);
                }
                // Re-check immediately after locking to avoid duplicate refreshes when another worker
                // repaired the cache between our earlier retry and lock acquisition.
                let (locked_retry_playlist, locked_retry_error) = load_cached_input_playlist(ctx, input).await;
                if locked_retry_error.is_none() {
                    warn!(
                        "Input '{}' cache became readable after lock re-check; skipping refresh",
                        input.name
                    );
                    preloaded_playlist = Some((locked_retry_playlist, None));
                } else {
                    warn!(
                        "Input '{}' cached playlist remained unreadable after retry and lock re-check; invalidating cache and forcing refresh",
                        input.name
                    );
                    invalidate_input_cache_status(ctx, input).await;
                    playlist_download_result = playlist_download_from_input(
                        &ctx.client,
                        &ctx.config,
                        input,
                        ctx.stalker_refresh_mode,
                    )
                    .await;
                }
            }
        } else {
            preloaded_playlist = Some((cached_playlist, None));
        }
    }
    if playlist_download_result.partial {
        ctx.partial_refresh.store(true, std::sync::atomic::Ordering::Release);
        if let Some(events) = ctx.event_manager.as_deref() {
            events.send_event(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
                target: input.name.to_string(),
                message: stalker_checkpoint_message(&input.name),
            }));
        }
    }
    let apply_staged_overlay = should_apply_staged_overlay(&playlist_download_result);

    let (mut playlist, mut error) = if let Some(preloaded) = preloaded_playlist {
        preloaded
    } else if playlist_download_result.was_cached || playlist_download_result.persisted {
        match load_input_playlist(ctx, input, None).await {
            Ok(pl_source) => (pl_source, None),
            Err(e) => (MemoryPlaylistSource::default().into_source(), Some(e)),
        }
    } else {
        debug!("Persisting input '{}' playlist", input.name);
        let (pl, err) = persist_input_playlist(&ctx.config, input, playlist_download_result.downloaded_playlist).await;
        (MemoryPlaylistSource::new(pl).into_source(), err)
    };

    playlist = filter_skipped_clusters_from_source(playlist, input);

    if let Some(staged_input) = staged_overlay.filter(|_| apply_staged_overlay) {
        let clusters = staged_input.staged.as_ref().map_or_else(ClusterFlags::all, |staged| staged.clusters);
        let (mut staged_download_err, mut staged_playlist, staged_error, staged_partial) =
            Box::pin(download_input(ctx, &staged_input, true)).await;
        playlist_download_result.partial |= staged_partial;
        playlist_download_result.download_err.append(&mut staged_download_err);
        if let Some(staged_error) = staged_error {
            playlist_download_result.download_err.push(staged_error);
        } else {
            let provider_groups = playlist.take_groups();
            let staged_groups = staged_playlist.take_groups();
            let merged_groups = apply_staged_overlay_groups(&input.name, clusters, provider_groups, staged_groups);
            let (merged_playlist, persist_error) = persist_input_playlist(&ctx.config, input, merged_groups).await;
            playlist = MemoryPlaylistSource::new(merged_playlist).into_source();
            if error.is_none() {
                error = persist_error;
            } else if let Some(persist_error) = persist_error {
                playlist_download_result.download_err.push(persist_error);
            }
        }
    }

    if mark_as_processed && !playlist_download_result.partial && error.is_none() && !playlist.is_empty() {
        // Mark after persist/load so other workers only see this input as ready when data is usable.
        ctx.mark_input_downloaded(input.name.clone()).await;
    }

    // Explicitly release per-input lock after load/persist/mark steps are completed.
    drop(input_lock);

    (playlist_download_result.download_err, playlist, error, playlist_download_result.partial)
}

fn create_broadcast_callback(event_manager: Option<&Arc<EventManager>>) -> StepMeasureCallback {
    if let Some(event_mgr) = event_manager {
        let events = event_mgr.clone();
        Box::new(move |context: &str, msg: &str| {
            events.send_event(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
                target: context.to_owned(),
                message: msg.to_owned(),
            }));
        })
    } else {
        Box::new(move |_context: &str, _msg: &str| { /* noop */ })
    }
}

fn create_input_stat(
    group_count: usize,
    channel_count: usize,
    error_count: usize,
    input_type: InputType,
    input_name: &str,
    secs_took: u64,
) -> InputStats {
    InputStats {
        name: input_name.to_string(),
        input_type,
        error_count,
        raw_stats: PlaylistStats { group_count, channel_count },
        processed_stats: PlaylistStats { group_count: 0, channel_count: 0 },
        secs_took,
    }
}

#[derive(Clone)]
pub struct PlaylistProcessingContext {
    pub client: reqwest::Client,
    pub config: Arc<AppConfig>,
    pub user_targets: Arc<ProcessTargets>,
    pub event_manager: Option<Arc<EventManager>>,
    pub playlist_state: Option<Arc<PlaylistStorageState>>,
    pub disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,

    // Coordination
    pub processed_inputs: Arc<Mutex<HashSet<Arc<str>>>>,
    #[allow(clippy::type_complexity)]
    pub input_locks: Arc<Mutex<HashMap<Arc<str>, Weak<RwLock<()>>>>>,

    // New field for STRM probes & background updates
    pub provider_manager: Option<Arc<ActiveProviderManager>>,
    pub metadata_manager: Option<Arc<MetadataUpdateManager>>,
    pub pre_processed_inputs: Option<Arc<HashSet<Arc<str>>>>,
    pub stalker_refresh_mode: StalkerRefreshMode,
    pub partial_refresh: Arc<std::sync::atomic::AtomicBool>,
}

impl PlaylistProcessingContext {
    pub async fn is_input_downloaded(&self, input_name: &str) -> bool {
        let processed = self.processed_inputs.lock().await;
        processed.contains(input_name)
    }
    pub async fn mark_input_downloaded(&self, input_name: Arc<str>) -> bool {
        let mut processed = self.processed_inputs.lock().await;
        processed.insert(input_name)
    }

    pub async fn get_input_lock(&self, input_name: &Arc<str>) -> OwnedRwLockWriteGuard<()> {
        let mut locks = self.input_locks.lock().await;
        // Try to upgrade the existing weak reference
        let lock = locks.get(input_name).and_then(Weak::upgrade).unwrap_or_else(|| {
            let new_lock = Arc::new(RwLock::new(()));
            locks.insert(input_name.clone(), Arc::downgrade(&new_lock));
            new_lock
        });

        // Clean up stale references periodically
        locks.retain(|_, weak| weak.strong_count() > 0);

        drop(locks); // Release mutex before awaiting write lock
        lock.write_owned().await
    }
}

async fn process_sources(processing_ctx: &PlaylistProcessingContext) -> (Vec<SourceStats>, Vec<TuliproxError>) {
    let mut async_tasks = JoinSet::new();
    let sources = processing_ctx.config.sources.load();
    let process_parallel = processing_ctx.config.config.load().process_parallel;
    if process_parallel && log_enabled!(Level::Debug) {
        debug!("Parallel processing enabled");
    }

    let mut source_results = Vec::new();
    let mut errors = Vec::new();
    let mut processed_any = false;

    for (index, source) in sources.sources.iter().enumerate() {
        if !source.should_process_for_user_targets(&processing_ctx.user_targets) {
            continue;
        }

        // We're using the file lock this way on purpose
        let source_lock_path = PathBuf::from(concat_string!("source_", &index.to_string()));
        let Ok(update_lock) = processing_ctx.config.file_locks.try_write_lock(&source_lock_path).await else {
            warn!("The update operation for the source at index {index} was skipped because an update is already in progress.");
            continue;
        };

        let ctx = Arc::new(processing_ctx.clone());

        processed_any = true;
        if process_parallel {
            async_tasks.spawn(async move {
                let _update_lock = update_lock;
                (index, process_source(index, ctx).await)
            });
        } else {
            source_results.push((index, process_source(index, ctx).await));
            drop(update_lock);
        }
    }
    if !processed_any {
        warn!(
            "No sources were processed for the given targets. Check that:\n\
             - Sources have enabled targets matching your target selection\n\
             - CLI -t filter or schedule.targets are correct\n\
             - No playlist lock is blocking updates"
        );
    }
    while let Some(result) = async_tasks.join_next().await {
        match result {
            Ok(result) => source_results.push(result),
            Err(err) => {
                error!("Playlist processing task failed: {err:?}");
                errors.push(TuliproxError::RepositoryPlaylist(format!(
                    "Playlist source processing task failed: {err}"
                )));
            }
        }
    }

    source_results.sort_by_key(|(index, _)| *index);
    let mut stats = Vec::with_capacity(source_results.len());
    for (_, (input_stats, target_stats, mut source_errors)) in source_results {
        errors.append(&mut source_errors);
        if let Some(source_stats) = SourceStats::try_new(input_stats, target_stats) {
            stats.push(source_stats);
        }
    }
    (stats, errors)
}

pub type ProcessingPipe = Vec<fn(source: &mut PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>>>;

fn get_processing_pipe(target: &ConfigTarget) -> ProcessingPipe {
    match &target.processing_order {
        ProcessingOrder::Frm => vec![filter_playlist, rename_playlist, map_playlist],
        ProcessingOrder::Fmr => vec![filter_playlist, map_playlist, rename_playlist],
        ProcessingOrder::Rfm => vec![rename_playlist, filter_playlist, map_playlist],
        ProcessingOrder::Rmf => vec![rename_playlist, map_playlist, filter_playlist],
        ProcessingOrder::Mfr => vec![map_playlist, filter_playlist, rename_playlist],
        ProcessingOrder::Mrf => vec![map_playlist, rename_playlist, filter_playlist],
    }
}

fn execute_pipe<'a>(
    target: &ConfigTarget,
    pipe: &ProcessingPipe,
    fpl: &mut FetchedPlaylist<'a>,
    duplicates: &mut HashSet<UUIDType>,
    consume_source: bool,
) -> Result<FetchedPlaylist<'a>, TuliproxError> {
    let source = if consume_source {
        if fpl.is_memory() {
            MemoryPlaylistSource::new(fpl.source.take_groups()).into_source()
        } else {
            std::mem::replace(&mut fpl.source, MemoryPlaylistSource::default().into_source())
        }
    } else {
        fpl.clone_source()?
    };

    let mut new_fpl = FetchedPlaylist { input: fpl.input, source, epg: fpl.epg.clone() };
    // In-memory items are frozen here at the target-processing boundary. Read-only disk sources
    // capture the same identity when their persisted M3U/Xtream items are converted to PlaylistItem.
    if new_fpl.is_memory() {
        for item in new_fpl.items_mut() {
            item.header.freeze_input_stream_id();
        }
    }
    if target.options.as_ref().is_some_and(|opt| opt.remove_duplicates) {
        new_fpl.deduplicate(duplicates);
    }

    for f in pipe {
        if let Some(groups) = f(&mut new_fpl.source, target) {
            new_fpl.source = MemoryPlaylistSource::new(groups).into_source();
        }
    }
    // Ensure source is memory-based for downstream mutable processing (VOD/series resolution)
    if !new_fpl.is_memory() {
        new_fpl.source = MemoryPlaylistSource::new(new_fpl.source.take_groups()).into_source();
    }
    Ok(new_fpl)
}

// This method is needed, because of duplicate group names in different inputs.
// We merge the same group names considering cluster together.
fn flatten_groups(playlistgroups: Vec<PlaylistGroup>) -> Vec<PlaylistGroup> {
    let upper_bound = playlistgroups.len();
    let mut sort_order: Vec<PlaylistGroup> = Vec::with_capacity(upper_bound);
    let mut idx: usize = 0;
    let mut group_map: HashMap<CategoryKey, usize> = HashMap::with_capacity(upper_bound);
    for group in playlistgroups {
        let normalized_title: Arc<str> = shared::utils::deunicode_string(&group.title).to_lowercase().intern();
        let key = (group.xtream_cluster, normalized_title);
        match group_map.entry(key) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(idx);
                idx += 1;
                sort_order.push(group);
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                if let Some(pl_group) = sort_order.get_mut(*o.get()) {
                    pl_group.channels.extend(group.channels);
                }
            }
        }
    }
    sort_order
}

struct PreparedTarget {
    target: ConfigTarget,
    playlist: Vec<PlaylistGroup>,
    epg: Vec<Epg>,
}

#[allow(clippy::too_many_arguments)]
async fn prepare_playlist_for_target(
    ctx: &PlaylistProcessingContext,
    playlists: &mut [FetchedPlaylist<'_>],
    target: &ConfigTarget,
    stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    consume_input_source: bool,
) -> Result<PreparedTarget, Vec<TuliproxError>> {
    debug_if_enabled!("Processing order is {}", &target.processing_order);
    log_memory_snapshot(format!("target '{}' start", target.name).as_str());

    let mut duplicates: HashSet<UUIDType> = HashSet::new();
    let mut new_epg = vec![];
    let mut new_playlist: Vec<PlaylistGroup> = vec![];

    debug!("Executing processing pipes");
    let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());

    let pipe = get_processing_pipe(target);
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    for provider_fpl in playlists.iter_mut() {
        log_memory_snapshot(
            format!("target '{}' input '{}' before_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        step.broadcast("Executing transformations on '{}' playlist", &target.name);
        let mut processed_fpl =
            execute_pipe(target, &pipe, provider_fpl, &mut duplicates, consume_input_source).map_err(|err| vec![err])?;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        processed_fpl.sort_by_provider_ordinal();
        playlist_resolve(ctx, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_vod_resolve", target.name, provider_fpl.input.name).as_str(),
        );
        process_playlist_epg(&mut processed_fpl, &mut new_epg).await;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_epg_apply", target.name, processed_fpl.input.name).as_str(),
        );
        let deduplicate = target.options.as_ref().is_some_and(|options| options.remove_duplicates);
        if let Some(groups) = map_playlist_at_stage(
            &mut processed_fpl.source,
            target,
            MappingStage::AfterEpg,
            deduplicate.then_some(&mut duplicates),
        ) {
            processed_fpl.source = MemoryPlaylistSource::new(groups).into_source();
        }
        if let Some(stat) = stats.get_mut(&processed_fpl.input.name) {
            stat.processed_stats.group_count = processed_fpl.get_group_count();
            stat.processed_stats.channel_count = processed_fpl.get_channel_count();
        }
        new_playlist.extend(processed_fpl.source.take_groups());
        log_memory_snapshot(
            format!("target '{}' input '{}' after_take_groups", target.name, processed_fpl.input.name).as_str(),
        );
        tokio::task::yield_now().await;
    }
    step.tick("filter rename map + epg");
    log_memory_snapshot(format!("target '{}' after_filter_rename_map_epg", target.name).as_str());
    step.stop("Preparing playlist");
    Ok(PreparedTarget { target: target.clone(), playlist: new_playlist, epg: new_epg })
}

/// Spill each `Epg` source to a temp `BPlusTree` and merge them. Extracted
/// from `finalize_prepared_target` so it can be unit-tested without
/// constructing a full `PlaylistProcessingContext`.
///
/// Returns `Ok(None)` if `sources` is empty (no EPG to merge), matching
/// the contract of `flatten_tvguide`. The temp directory lives inside
/// this function call — all temp files are removed by the
/// `DiskEpgSource` drop guards before this function returns.
fn spill_epg_to_disk(sources: Vec<Epg>) -> Result<Option<Epg>, TuliproxError> {
    let dir = tempfile::tempdir()
        .map_err(|e| TuliproxError::RepositoryXtream(format!("tempdir for EPG spill: {e}")))?;
    let mut disk_sources = Vec::with_capacity(sources.len());
    for (source_order, guide) in sources.into_iter().enumerate() {
        let mut acc = EpgMergeAccumulator::new();
        acc.set_attributes_if_preferred(guide.priority, source_order, guide.attributes);
        for channel in guide.children {
            acc.add_channel_with_programmes(
                guide.priority,
                source_order,
                guide.logo_override,
                std::sync::Arc::unwrap_or_clone(channel),
            );
        }
        let path = dir.path().join(format!("epg-src-{source_order}.db"));
        let source_order_u32 = u32::try_from(source_order).unwrap_or(0);
        let source = acc
            .finish_into_disk(path, guide.priority, source_order_u32)
            .map_err(|e| TuliproxError::RepositoryXtream(format!("EPG spill to disk failed: {e}")))?;
        disk_sources.push(source);
    }
    if disk_sources.is_empty() {
        Ok(None)
    } else {
        merge_epg_trees(disk_sources)
            .map_err(|e| TuliproxError::RepositoryXtream(format!("EPG disk merge failed: {e}")))
            .map(|opt| opt.map(|(epg, _)| epg))
    }
}

async fn finalize_prepared_target(
    ctx: Arc<PlaylistProcessingContext>,
    prepared: PreparedTarget,
) -> (Result<(), Vec<TuliproxError>>, Vec<TuliproxError>) {
    let target = &prepared.target;
    let mut new_playlist = prepared.playlist;
    let new_epg = prepared.epg;
    let mut errors = Vec::new();
    let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    if target.favourites.is_some() {
        step.broadcast("Processing favourites for '{}' playlist", &target.name);
        process_favourites(&mut new_playlist, target.favourites.as_deref());
        log_memory_snapshot(format!("target '{}' after_favourites", target.name).as_str());
    }

    if new_playlist.is_empty() {
        step.stop("");
        info!("Playlist is empty: {}", target.name);
        (Ok(()), errors)
    } else {
        // Process Trakt categories
        if trakt_playlist(&ctx.client, target, &mut errors, &mut new_playlist).await {
            step.tick("trakt categories");
            log_memory_snapshot(format!("target '{}' after_trakt", target.name).as_str());
        }

        let mut flat_new_playlist = flatten_groups(new_playlist);
        step.tick("playlist merge");
        log_memory_snapshot(format!("target '{}' after_playlist_merge", target.name).as_str());

        if let Some(dedup_config) = target.options.as_ref().and_then(|options| options.deduplicate.as_ref()) {
            let removed = crate::processing::processor::deduplicate::deduplicate_playlist(
                *dedup_config,
                &mut flat_new_playlist,
            );
            if removed > 0 {
                info!("Deduplicated {removed} channels for target {}", target.name);
            }
            step.tick("playlist dedup");
            log_memory_snapshot(format!("target '{}' after_playlist_dedup", target.name).as_str());
        }

        if sort_playlist(target, &mut flat_new_playlist) {
            step.tick("playlist sort");
            log_memory_snapshot(format!("target '{}' after_playlist_sort", target.name).as_str());
        }
        assign_channel_no_playlist(&mut flat_new_playlist);
        step.tick("assigning channel numbers");
        log_memory_snapshot(format!("target '{}' after_assign_channel_numbers", target.name).as_str());
        map_playlist_counter(target, &mut flat_new_playlist);
        step.tick("assigning channel counter");
        log_memory_snapshot(format!("target '{}' after_assign_channel_counter", target.name).as_str());

        if process_watch(&ctx.config, &ctx.client, target, &flat_new_playlist).await {
            step.tick("group watches");
            log_memory_snapshot(format!("target '{}' after_group_watches", target.name).as_str());
        }
        let merged_epg = if ctx.config.config.load().disk_based_processing {
            // Per-source drain to disk, then multi-way merge. Errors are pushed
            // to `errors` rather than `?` because the function returns
            // `(Result, Vec<TuliproxError>)`, not `Result` directly. We must
            // surface tempdir / write / merge failures — the user opted in to
            // disk spilling, and silently falling back to the in-memory path
            // can OOM on large feeds. When the spill itself fails we skip the
            // persist step entirely: continuing with `merged_epg = None` would
            // overwrite the existing on-disk EPG with nothing and discard the
            // previously persisted artifact on a transient error.
            match spill_epg_to_disk(new_epg) {
                Ok(epg) => epg,
                Err(err) => {
                    errors.push(err);
                    step.stop("EPG spill failed; skipping persist to preserve existing EPG");
                    log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
                    return (Ok(()), errors);
                }
            }
        } else {
            flatten_tvguide(new_epg)
        };
        let result = persist_playlist(
            &ctx.config,
            &mut flat_new_playlist,
            merged_epg.as_ref(),
            target,
            ctx.playlist_state.as_ref(),
        )
            .await;
        step.stop("Persisting playlists");
        log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
        (result, errors)
    }
}

async fn playlist_resolve(
    ctx: &PlaylistProcessingContext,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    pipe: &ProcessingPipe,
    provider_fpl: &mut FetchedPlaylist<'_>,
    processed_fpl: &mut FetchedPlaylist<'_>,
) {
    playlist_resolve_series(ctx, target, errors, pipe, provider_fpl, processed_fpl).await;
    playlist_resolve_vod(ctx, target, errors, provider_fpl, processed_fpl).await;
    playlist_probe(ctx, target, processed_fpl).await;
}

fn is_probe_supported_item_type(item_type: PlaylistItemType) -> bool {
    matches!(
        item_type,
        PlaylistItemType::Live // we skip other live streams because hls and dash have multiple resolutions
                | PlaylistItemType::Video
                | PlaylistItemType::LocalVideo
                | PlaylistItemType::Series
                | PlaylistItemType::LocalSeries
    )
}

fn has_probe_details(item: &PlaylistItem) -> bool {
    match item.header.additional_properties.as_ref() {
        Some(StreamProperties::Video(v)) => v.details.as_ref().is_some_and(|d| d.video.is_some() && d.audio.is_some()),
        Some(StreamProperties::Live(l)) => l.video.is_some() && l.audio.is_some() && l.bitrate > 0,
        Some(StreamProperties::Episode(e)) => e.video.is_some() && e.audio.is_some(),
        Some(StreamProperties::Series(_)) | None => false,
    }
}

fn get_live_probe_interval_settings(
    target: &ConfigTarget,
    input_type: InputType,
    input_options: Option<&ConfigInputOptions>,
) -> Option<(u16, u64)> {
    if !(input_type.is_xtream() || input_type.is_m3u() || input_type.is_stalker()) {
        return None;
    }
    target.get_xtream_output().map(|_| {
        let (probe_delay, input_probe_live_interval_hours) = input_options
            .map_or((default_probe_delay_secs(), default_probe_live_interval()), |options| {
                (options.probe_delay, options.probe_live_interval_hours)
            });
        (probe_delay, u64::from(input_probe_live_interval_hours) * 3600)
    })
}

fn needs_live_probe(item: &PlaylistItem, cutoff_ts: i64) -> bool {
    match item.header.additional_properties.as_ref() {
        Some(StreamProperties::Live(props)) => {
            props.bitrate == 0 || props.last_probed_timestamp.is_none_or(|last_ts| last_ts < cutoff_ts)
        }
        _ => true,
    }
}

fn provider_id_from_item(item: &PlaylistItem) -> Option<ProviderIdType> {
    if let Ok(id) = item.header.id.parse::<u32>() {
        if id == 0 {
            return None;
        }
        return Some(ProviderIdType::Id(id));
    }

    let raw = item.header.id.trim();
    if raw.is_empty() {
        None
    } else {
        Some(ProviderIdType::from(raw))
    }
}

#[allow(clippy::too_many_lines)]
async fn playlist_probe(ctx: &PlaylistProcessingContext, target: &ConfigTarget, fpl: &mut FetchedPlaylist<'_>) {
    let Some(mgr) = ctx.metadata_manager.as_ref() else {
        return;
    };
    let Some(opts) = fpl.input.options.as_ref() else {
        return;
    };
    let probe_live_enabled = opts.has_flag(ConfigInputFlags::ProbeLive);
    let probe_vod_enabled = opts.has_flag(ConfigInputFlags::ProbeVod);
    let probe_series_enabled = opts.has_flag(ConfigInputFlags::ProbeSeries);

    if !(probe_live_enabled || probe_vod_enabled || probe_series_enabled) {
        return;
    }
    if !ctx.config.is_ffprobe_enabled().await {
        return;
    }

    let input_name = fpl.input.name.clone();
    let effective_input_type = fpl.input.get_download_input_type();
    let xtream_probe_handled = effective_input_type.is_xtream() && target.get_xtream_output().is_some();
    let live_probe_settings = if probe_live_enabled {
        get_live_probe_interval_settings(target, effective_input_type, Some(opts)).map(|(delay, interval_secs)| {
            let interval_signed = i64::try_from(interval_secs).unwrap_or(i64::MAX);
            let cutoff_ts = chrono::Utc::now().timestamp().saturating_sub(interval_signed);
            (delay, interval_secs, cutoff_ts)
        })
    } else {
        None
    };

    let mut queued_probe_keys: HashSet<(Arc<str>, String)> = HashSet::new();
    let mut queued_live_keys: HashSet<ProviderIdType> = HashSet::new();
    let mut queued_live_count = 0usize;
    let mut queued_stream_count = 0usize;

    let probe_filter = fpl.input.options.as_ref().and_then(|o| o.probe_filter.as_ref());

    for item in fpl.items() {

        if !is_probe_supported_item_type(item.header.item_type) {
            continue;
        }
        match item.header.item_type {
            PlaylistItemType::Live => {
                if !probe_live_enabled {
                    continue;
                }
            }
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
                if !probe_vod_enabled {
                    continue;
                }
            }
            PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                if !probe_series_enabled {
                    continue;
                }
            }
            _ => continue,
        }

        // If input has a probe filter and this item doesn't match, skip probing
        if let Some(p_filter) = probe_filter {
            let provider = ValueProvider { pli: &item, match_as_ascii: false };
            if !p_filter.filter(&provider) {
                continue;
            }
        }

        match item.header.item_type {
            PlaylistItemType::Live => {
                if let Some((probe_delay, interval_secs, cutoff_ts)) = live_probe_settings {
                    if needs_live_probe(&item, cutoff_ts) {
                        if let Some(provider_id) = provider_id_from_item(&item) {
                            if queued_live_keys.insert(provider_id.clone()) {
                                let task = UpdateTask::ProbeLive {
                                    id: provider_id.clone(),
                                    reason: ResolveReason::Probe.into(),
                                    delay: probe_delay,
                                    interval: interval_secs,
                                };
                                if mgr.should_skip_enqueue(input_name.clone(), &task).await {
                                    continue;
                                }
                                if log_enabled!(Level::Debug) {
                                    let last_probed = match item.header.additional_properties.as_ref() {
                                        Some(StreamProperties::Live(props)) => props.last_probed_timestamp,
                                        _ => None,
                                    };
                                    debug!(
                                        "[Task] Creating ProbeLive task for input {}: id={}, last_probed_ts={:?}, cutoff_ts={}, interval={}s, title=\"{}\"",
                                        input_name, provider_id, last_probed, cutoff_ts, interval_secs, item.header.title
                                    );
                                }
                                mgr.queue_task_background(input_name.clone(), task);
                                queued_live_count += 1;
                            }
                        }
                    }
                    continue;
                }
                // If live probes are enabled but no live-specific settings are available, fall through to the
                // generic probe path to keep behaviour consistent with non-xtream outputs.
            }
            PlaylistItemType::Video | PlaylistItemType::LocalVideo => {
                // Xtream outputs handle VOD probe as part of the resolve pipeline (after resolve).
                if xtream_probe_handled {
                    continue;
                }
            }
            PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                // Xtream outputs handle Series probe as part of the resolve pipeline (after resolve).
                if xtream_probe_handled {
                    continue;
                }
            }
            _ => continue,
        }

        if has_probe_details(&item) {
            continue;
        }

        // For M3U, ID is a provider id; for Library, ID is UUID.
        let unique_id = if effective_input_type == InputType::Library {
            item.header.uuid.to_valid_uuid()
        } else {
            item.header.id.to_string()
        };
        let probe_scope =
            if item.header.input_name.is_empty() { input_name.clone() } else { item.header.input_name.clone() };

        if !queued_probe_keys.insert((probe_scope.clone(), unique_id.clone())) {
            continue;
        }

        let task = UpdateTask::ProbeStream {
            probe_scope: probe_scope.clone(),
            unique_id: unique_id.clone(),
            url: item.header.url.to_string(),
            item_type: item.header.item_type,
            reason: ResolveReason::MissingDetails.into(),
            delay: opts.probe_delay,
        };
        if mgr.should_skip_enqueue(input_name.clone(), &task).await {
            continue;
        }
        debug!(
            "[Task] Creating ProbeStream task for input {}: scope={}, unique_id={}, item_type={:?}, title=\"{}\"",
            input_name, probe_scope, unique_id, item.header.item_type, item.header.title
        );
        mgr.queue_task_background(input_name.clone(), task);
        queued_stream_count += 1;
    }

    if queued_live_count > 0 || queued_stream_count > 0 {
        info!("Queued probe tasks for input {input_name} (live_interval={queued_live_count}, generic={queued_stream_count})");
    }
}

pub fn process_favourites(playlist: &mut Vec<PlaylistGroup>, favourites_cfg: Option<&[ConfigFavourites]>) {
    if let Some(favourites) = favourites_cfg {
        let mut fav_groups: IndexMap<CategoryKey, Vec<PlaylistItem>> = IndexMap::new();
        for pg in playlist.iter() {
            for pli in &pg.channels {
                // series episodes can't be included in favourites
                if pli.header.item_type == PlaylistItemType::Series
                    || pli.header.item_type == PlaylistItemType::LocalSeries
                {
                    continue;
                }
                for fav in favourites {
                    if pli.header.xtream_cluster == fav.cluster && is_valid(pli, &fav.filter, fav.match_as_ascii) {
                        let mut channel = pli.clone();
                        channel.header.group.clone_from(&fav.group);
                        // Update UUID to be an alias of the original
                        channel.header.uuid = create_alias_uuid(&pli.header.uuid, &fav.group);
                        fav_groups.entry((fav.cluster, fav.group.clone())).or_default().push(channel);
                    }
                }
            }
        }

        for (fav_group, channels) in fav_groups {
            if !channels.is_empty() {
                let (xtream_cluster, group_name) = fav_group;
                playlist.push(PlaylistGroup { id: 0, title: group_name, channels, xtream_cluster });
            }
        }
    }
}

async fn trakt_playlist(
    client: &reqwest::Client,
    target: &ConfigTarget,
    errors: &mut Vec<TuliproxError>,
    playlist: &mut Vec<PlaylistGroup>,
) -> bool {
    match process_trakt_categories_for_target(client, playlist, target).await {
        Ok(Some(trakt_categories)) => {
            if !trakt_categories.is_empty() {
                info!("Adding {} Trakt categories to playlist", trakt_categories.len());
                playlist.extend(trakt_categories);
            }
        }
        Ok(None) => {
            return false;
        }
        Err(trakt_errors) => {
            warn!("Trakt processing failed with {} errors", trakt_errors.len());
            errors.extend(trakt_errors);
        }
    }
    true
}

async fn process_watch(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    target: &ConfigTarget,
    new_playlist: &[PlaylistGroup],
) -> bool {
    if let Some(watches) = &target.watch {
        if default_as_default().eq_ignore_ascii_case(&target.name) {
            error!("can't watch a target with no unique name");
            return false;
        }

        futures::stream::iter(
            new_playlist
                .iter()
                .filter(|pl| watches.iter().any(|r| r.is_match(&pl.title)))
                .map(|pl| process_group_watch(app_config, client, &target.name, pl)),
        )
            .for_each_concurrent(16, |f| f)
            .await;

        true
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn exec_processing(
    client: &reqwest::Client,
    app_config: Arc<AppConfig>,
    targets: Arc<ProcessTargets>,
    event_manager: Option<Arc<EventManager>>,
    app_state: Option<Arc<AppState>>,
    playlist_state: Option<Arc<PlaylistStorageState>>,
    update_guard: Option<UpdateGuard>,
    disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,
    provider_manager: Option<Arc<ActiveProviderManager>>,
    metadata_manager: Option<Arc<MetadataUpdateManager>>,
    pre_processed_inputs: Option<HashSet<Arc<str>>>,
    acquired_permit: Option<crate::api::model::UpdateGuardPermit>,
) {
    let max_update_duration = Duration::from_secs(PLAYLIST_UPDATE_MAX_DURATION_SECS);
    let playlist_guard = if let Some(permit) = acquired_permit {
        Some(permit)
    } else if let Some(guard) = &update_guard {
        if let Some(permit) = guard.acquire_playlist_lock().await {
            Some(permit)
        } else {
            warn!("Playlist update lock is closed; update skipped.");
            if let Some(events) = event_manager.as_deref() {
                events.send_event(EventMessage::PlaylistUpdate(shared::model::PlaylistUpdateState::Failure));
            }
            return;
        }
    } else {
        None
    };

    if playlist_guard.is_some() {
        if let Some(state) = app_state.as_ref() {
            if tokio::time::timeout(max_update_duration, sync_panel_api_exp_dates(state)).await.is_err() {
                error!(
                    "Playlist update bootstrap timed out after {PLAYLIST_UPDATE_MAX_DURATION_SECS} secs while holding playlist lock",
                );
                if let Some(events) = event_manager.as_deref() {
                    events.send_event(EventMessage::PlaylistUpdate(shared::model::PlaylistUpdateState::Failure));
                }
                return;
            }
        }
    }

    // Pause background metadata/probe tasks for the full update lifecycle.
    let _background_pause_guard = if let Some(manager) = metadata_manager.as_ref() {
        Some(manager.acquire_update_pause_guard().await)
    } else {
        None
    };

    info!("🌷 Update process started.");

    log_memory_snapshot("exec_processing start");

    // Initialize Context
    let ctx = PlaylistProcessingContext {
        client: client.clone(),
        config: app_config.clone(),
        user_targets: targets.clone(),
        event_manager: event_manager.clone(),
        playlist_state: playlist_state.clone(),
        processed_inputs: Arc::new(Mutex::new(HashSet::new())),
        input_locks: Arc::new(Mutex::new(HashMap::new())),
        disabled_headers,
        provider_manager,
        metadata_manager,
        pre_processed_inputs: pre_processed_inputs.map(Arc::new),
        stalker_refresh_mode: if app_config.config.load().process_parallel {
            StalkerRefreshMode::Parallel
        } else if update_guard.is_some() {
            StalkerRefreshMode::ServerSlice
        } else {
            StalkerRefreshMode::Complete
        },
        partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let start_time = Instant::now();
    let process_result = tokio::time::timeout(
        max_update_duration,
        std::panic::AssertUnwindSafe(process_sources(&ctx)).catch_unwind(),
    )
        .await;
    let (stats, errors) = match process_result {
        Ok(Ok((stats, errors))) => (stats, errors),
        Ok(Err(_)) => {
            error!("Playlist processing panicked");
            if let Some(events) = event_manager.as_deref() {
                events.send_event(EventMessage::PlaylistUpdate(shared::model::PlaylistUpdateState::Failure));
            }
            return;
        }
        Err(_) => {
            error!(
                "Playlist processing timed out after {PLAYLIST_UPDATE_MAX_DURATION_SECS} secs while holding playlist lock",
            );
            if let Some(events) = event_manager.as_deref() {
                events.send_event(EventMessage::PlaylistUpdate(shared::model::PlaylistUpdateState::Failure));
            }
            return;
        }
    };
    log_memory_snapshot("exec_processing after_process_sources");

    // Keep the update lock only for the critical processing section.
    drop(playlist_guard);
    debug!("Released playlist update lock; dispatching notifications and events");

    // log errors
    for err in &errors {
        error!("{}", err.message());
    }

    if !stats.is_empty() {
        // print stats
        if let Ok(stats_msg) = serde_json::to_string(&stats) {
            info!("stats: {stats_msg}");
        }
        // send stats
        send_message(&app_config, client, MessageContent::event_stats(stats)).await;
    }

    // send errors
    if let Some(message) = get_errors_notify_message!(errors, 255) {
        if let Some(events) = event_manager.as_deref() {
            events.send_event(EventMessage::PlaylistUpdate(shared::model::PlaylistUpdateState::Failure));
        }
        send_message(&app_config, client, MessageContent::event_error(message)).await;
    } else if let Some(events) = event_manager.as_deref() {
        let update_state = if ctx.partial_refresh.load(std::sync::atomic::Ordering::Acquire) {
            shared::model::PlaylistUpdateState::Partial
        } else {
            shared::model::PlaylistUpdateState::Success
        };
        events.send_event(EventMessage::PlaylistUpdate(update_state));
    }

    let elapsed = start_time.elapsed().as_secs();
    let update_finished_message = format!("🌷 Update process finished! Took {elapsed} secs.");

    if let Some(events) = event_manager.as_deref() {
        events.send_event(EventMessage::PlaylistUpdateProgress(PlaylistUpdateProgressEvent {
            target: "Playlist Update".to_string(),
            message: update_finished_message.clone(),
        }));
    }
    log_memory_snapshot("exec_processing before_interner_gc");
    debug!("StringInterner GC removed {} strings", interner_gc());
    log_memory_snapshot("exec_processing after_interner_gc");
    //trim_allocator_after_update();

    info!("{update_finished_message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::foundation::{get_filter, MapperScript, ValueProvider};
    use shared::model::{
        ClusterFlags, ConfigInputDto, ConfigRenameDto, ConfigTargetDto, ConfigTargetOptions, M3uPlaylistItem,
        MappingStage, PlaylistEntry, PlaylistItem, PlaylistItemHeader, PlaylistItemType, XtreamCluster,
        XtreamPlaylistItem,
    };
    use shared::utils::Internable;
    use crate::model::Config;

    fn serialize_without_trailing_fields<T: serde::Serialize>(value: &T, trailing_fields: &[u8]) -> Vec<u8> {
        let mut encoded = rmp_serde::to_vec(value).expect("playlist item should serialize");
        for expected in trailing_fields {
            assert_eq!(encoded.pop(), Some(*expected), "unexpected trailing MessagePack field");
        }
        let removed = trailing_fields.len();
        match encoded[0] {
            marker @ 0x92..=0x9f => {
                let len = usize::from(marker - 0x90);
                assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
                encoded[0] = 0x90 + u8::try_from(len - removed).unwrap_or_default();
            }
            0xdc => {
                let len = u16::from_be_bytes([encoded[1], encoded[2]]);
                let removed = u16::try_from(removed).unwrap_or(u16::MAX);
                assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
                encoded[1..3].copy_from_slice(&(len - removed).to_be_bytes());
            }
            0xdd => {
                let len = u32::from_be_bytes([encoded[1], encoded[2], encoded[3], encoded[4]]);
                let removed = u32::try_from(removed).unwrap_or(u32::MAX);
                assert!(len >= removed, "trailing field count exceeds MessagePack sequence length");
                encoded[1..5].copy_from_slice(&(len - removed).to_be_bytes());
            }
            marker => panic!("unexpected MessagePack sequence marker {marker:#x}"),
        }
        encoded
    }

    fn item_with_props(props: StreamProperties) -> PlaylistItem {
        let header = shared::model::PlaylistItemHeader { additional_properties: Some(props), ..Default::default() };
        PlaylistItem { header }
    }

    fn live_item_with_probe_timestamp_and_bitrate(last_probed_timestamp: i64, bitrate: u32) -> PlaylistItem {
        item_with_props(StreamProperties::Live(Box::new(shared::model::LiveStreamProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: Some("{\"codec_name\":\"aac\"}".intern()),
            bitrate,
            last_probed_timestamp: Some(last_probed_timestamp),
            ..Default::default()
        })))
    }

    #[test]
    fn rename_preserves_input_stream_id_captured_at_target_boundary() {
        let mut item = PlaylistItem {
            header: PlaylistItemHeader {
                id: "origin-alpha".intern(),
                url: "http://provider.example/channel.m3u8".intern(),
                ..Default::default()
            },
        };
        item.header.freeze_input_stream_id();
        let rename = ConfigRename::from(&ConfigRenameDto {
            field: ItemField::Url,
            pattern: "provider".to_string(),
            new_name: "target".to_string(),
            t_pattern: None,
        });

        exec_rename(&mut item, Some(&vec![rename]));

        assert_eq!(item.header.url.as_ref(), "http://target.example/channel.m3u8");
        assert_eq!(item.header.input_stream_id.as_ref(), "origin-alpha");
    }

    #[test]
    fn mapper_changes_id_without_changing_frozen_input_stream_id() {
        let mut item = PlaylistItem {
            header: PlaylistItemHeader {
                id: "origin-alpha".intern(),
                name: "Channel".intern(),
                ..Default::default()
            },
        };
        item.header.freeze_input_stream_id();
        let mapping = Mapping {
            mapper: Some(vec![crate::model::Mapper {
                filter: r#"name ~ ".*""#.to_string(),
                script: r#"@id = "target-id""#.to_string(),
                t_filter: Some(get_filter(r#"name ~ ".*""#, None).expect("filter should parse")),
                t_script: Some(MapperScript::parse(r#"@id = "target-id""#, None).expect("mapper should parse")),
            }]),
            ..Default::default()
        };

        let (mapped, _, matched) = map_channel(item, &mapping);

        assert!(matched);
        assert_eq!(mapped.header.id.as_ref(), "target-id");
        assert_eq!(mapped.header.input_stream_id.as_ref(), "origin-alpha");
    }

    #[test]
    fn mapper_cannot_resurrect_missing_legacy_input_stream_id_from_target_id() {
        let mut source = PlaylistItem {
            header: PlaylistItemHeader {
                id: "80510".intern(),
                url: "http://provider.example/live/user/pass/80510.ts".intern(),
                input_name: "input".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..Default::default()
            },
        };
        source.header.freeze_input_stream_id();
        let mut legacy_xtream = XtreamPlaylistItem::from(&source);
        legacy_xtream.provider_id = 0;
        legacy_xtream.input_stream_id = "".intern();
        legacy_xtream.url = "http://provider.example/live/channel.m3u8".intern();
        let mut legacy_item = PlaylistItem::from(&legacy_xtream);
        legacy_item.header.freeze_input_stream_id();
        let mapping = Mapping {
            mapper: Some(vec![crate::model::Mapper {
                filter: r#"name ~ ".*""#.to_string(),
                script: r#"@id = "target-id""#.to_string(),
                t_filter: Some(get_filter(r#"name ~ ".*""#, None).expect("filter should parse")),
                t_script: Some(MapperScript::parse(r#"@id = "target-id""#, None).expect("mapper should parse")),
            }]),
            ..Default::default()
        };

        let (mapped, _, matched) = map_channel(legacy_item, &mapping);
        let materialized_m3u = M3uPlaylistItem::from(&mapped);
        let materialized_xtream = XtreamPlaylistItem::from(&mapped);

        assert!(matched);
        assert_eq!(mapped.header.id.as_ref(), "target-id");
        assert_eq!(mapped.get_input_stream_id(), None);
        assert!(materialized_m3u.provider_id.is_empty());
        assert_eq!(materialized_m3u.get_input_stream_id(), None);
        assert_eq!(materialized_xtream.provider_id, 0);
        assert_eq!(materialized_xtream.get_input_stream_id(), None);
    }

    #[test]
    fn execute_pipe_freezes_input_stream_id_without_rename_or_mapper() {
        let input = ConfigInput::default();
        let item = PlaylistItem {
            header: PlaylistItemHeader { id: "origin-alpha".intern(), ..Default::default() },
        };
        let source = MemoryPlaylistSource::new(vec![PlaylistGroup {
            id: 1,
            title: "Group".intern(),
            channels: vec![item],
            xtream_cluster: XtreamCluster::Live,
        }])
        .into_source();
        let mut fetched = FetchedPlaylist { input: &input, source, epg: None };
        let mut duplicates = HashSet::new();
        let target = ConfigTarget::from(&ConfigTargetDto::default());

        let mut processed = execute_pipe(&target, &vec![], &mut fetched, &mut duplicates, false)
            .expect("target processing should succeed");
        let mut groups = processed.source.take_groups();

        assert_eq!(groups[0].channels[0].header.input_stream_id.as_ref(), "origin-alpha");
        assert!(groups[0].channels[0].header.set_field("id", "late-target-id"));
        assert_eq!(groups[0].channels[0].header.input_stream_id.as_ref(), "origin-alpha");
    }

    #[test]
    fn legacy_messagepack_playlist_items_default_missing_input_stream_id() {
        let mut source = PlaylistItem {
            header: PlaylistItemHeader {
                id: "origin-alpha".intern(),
                url: "http://provider.example/live/user/pass/80510.ts".intern(),
                input_name: "input".intern(),
                item_type: PlaylistItemType::Live,
                xtream_cluster: XtreamCluster::Live,
                ..Default::default()
            },
        };

        let header_bytes = serialize_without_trailing_fields(&source.header, &[0xc0, 0xa0]);
        let decoded_header: PlaylistItemHeader =
            rmp_serde::from_slice(&header_bytes).expect("legacy header should deserialize");
        assert!(decoded_header.input_stream_id.is_empty());
        assert_eq!(decoded_header.get_input_stream_id(), None);
        assert_eq!(decoded_header.upstream_user_agent, None);
        let mut decoded_header = decoded_header;
        decoded_header.freeze_input_stream_id();
        assert_eq!(decoded_header.get_input_stream_id().as_deref(), Some("origin-alpha"));

        source.header.freeze_input_stream_id();
        let mut m3u_item = M3uPlaylistItem::from(&source);
        m3u_item.input_stream_id = "".intern();
        let m3u_bytes = serialize_without_trailing_fields(&m3u_item, &[0xc0, 0xa0]);
        let decoded_m3u: M3uPlaylistItem =
            rmp_serde::from_slice(&m3u_bytes).expect("legacy M3U item should deserialize");
        assert!(decoded_m3u.input_stream_id.is_empty());
        assert_eq!(decoded_m3u.get_input_stream_id().as_deref(), Some("origin-alpha"));
        assert_eq!(decoded_m3u.upstream_user_agent, None);

        let mut xtream_item = XtreamPlaylistItem::from(&source);
        xtream_item.input_stream_id = "".intern();
        let xtream_bytes = serialize_without_trailing_fields(&xtream_item, &[0xc0, 0xa0]);
        let decoded_xtream: XtreamPlaylistItem =
            rmp_serde::from_slice(&xtream_bytes).expect("legacy Xtream item should deserialize");
        assert!(decoded_xtream.input_stream_id.is_empty());
        assert_eq!(decoded_xtream.get_input_stream_id().as_deref(), Some("80510"));
        assert_eq!(decoded_xtream.upstream_user_agent, None);
    }

    #[test]
    fn previous_messagepack_playlist_items_default_missing_upstream_user_agent() {
        let source = PlaylistItem {
            header: PlaylistItemHeader {
                id: "80510".intern(),
                input_stream_id: "origin-alpha".intern(),
                ..Default::default()
            },
        };

        let header: PlaylistItemHeader = rmp_serde::from_slice(&serialize_without_trailing_fields(&source.header, &[0xc0]))
            .expect("previous header should deserialize");
        let m3u: M3uPlaylistItem = rmp_serde::from_slice(&serialize_without_trailing_fields(
            &M3uPlaylistItem::from(&source),
            &[0xc0],
        ))
        .expect("previous M3U item should deserialize");
        let xtream: XtreamPlaylistItem = rmp_serde::from_slice(&serialize_without_trailing_fields(
            &XtreamPlaylistItem::from(&source),
            &[0xc0],
        ))
        .expect("previous Xtream item should deserialize");

        assert_eq!(header.input_stream_id.as_ref(), "origin-alpha");
        assert_eq!(m3u.input_stream_id.as_ref(), "origin-alpha");
        assert_eq!(xtream.input_stream_id.as_ref(), "origin-alpha");
        assert_eq!(header.upstream_user_agent, None);
        assert_eq!(m3u.upstream_user_agent, None);
        assert_eq!(xtream.upstream_user_agent, None);
    }

    #[test]
    fn messagepack_playlist_items_preserve_upstream_user_agent() -> Result<(), Box<dyn std::error::Error>> {
        let source = PlaylistItem {
            header: PlaylistItemHeader {
                upstream_user_agent: Some("Provider-UA".intern()),
                ..Default::default()
            },
        };

        let header: PlaylistItemHeader = rmp_serde::from_slice(&rmp_serde::to_vec(&source.header)?)?;
        let m3u: M3uPlaylistItem =
            rmp_serde::from_slice(&rmp_serde::to_vec(&M3uPlaylistItem::from(&source))?)?;
        let xtream: XtreamPlaylistItem =
            rmp_serde::from_slice(&rmp_serde::to_vec(&XtreamPlaylistItem::from(&source))?)?;

        assert_eq!(header.upstream_user_agent.as_deref(), Some("Provider-UA"));
        assert_eq!(m3u.upstream_user_agent.as_deref(), Some("Provider-UA"));
        assert_eq!(xtream.upstream_user_agent.as_deref(), Some("Provider-UA"));
        Ok(())
    }

    #[test]
    fn has_probe_details_requires_video_and_audio_for_video() {
        let video = shared::model::VideoStreamProperties {
            details: Some(shared::model::VideoStreamDetailProperties {
                video: Some("{\"codec_name\":\"h264\"}".intern()),
                audio: None,
                ..Default::default()
            }),
            ..Default::default()
        };
        let item_missing_audio = item_with_props(StreamProperties::Video(Box::new(video)));
        assert!(!has_probe_details(&item_missing_audio));

        let video_complete = shared::model::VideoStreamProperties {
            details: Some(shared::model::VideoStreamDetailProperties {
                video: Some("{\"codec_name\":\"h264\"}".intern()),
                audio: Some("{\"codec_name\":\"aac\"}".intern()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let item_complete = item_with_props(StreamProperties::Video(Box::new(video_complete)));
        assert!(has_probe_details(&item_complete));
    }

    #[test]
    fn has_probe_details_requires_video_audio_and_bitrate_for_live() {
        let live_missing_audio = shared::model::LiveStreamProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: None,
            ..Default::default()
        };
        let item_missing_audio = item_with_props(StreamProperties::Live(Box::new(live_missing_audio)));
        assert!(!has_probe_details(&item_missing_audio));

        let live_missing_bitrate = shared::model::LiveStreamProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: Some("{\"codec_name\":\"aac\"}".intern()),
            ..Default::default()
        };
        let item_missing_bitrate = item_with_props(StreamProperties::Live(Box::new(live_missing_bitrate)));
        assert!(!has_probe_details(&item_missing_bitrate));

        let live_complete = shared::model::LiveStreamProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: Some("{\"codec_name\":\"aac\"}".intern()),
            bitrate: 2_500_000,
            ..Default::default()
        };
        let item_complete = item_with_props(StreamProperties::Live(Box::new(live_complete)));
        assert!(has_probe_details(&item_complete));
    }

    #[test]
    fn needs_live_probe_when_fresh_probe_has_no_bitrate() {
        let item = live_item_with_probe_timestamp_and_bitrate(101, 0);

        assert!(needs_live_probe(&item, 100));
    }

    #[test]
    fn does_not_need_live_probe_when_fresh_probe_has_positive_bitrate() {
        let item = live_item_with_probe_timestamp_and_bitrate(101, 2_500_000);

        assert!(!needs_live_probe(&item, 100));
    }

    #[test]
    fn needs_live_probe_when_positive_bitrate_probe_is_older_than_cutoff() {
        let item = live_item_with_probe_timestamp_and_bitrate(99, 2_500_000);

        assert!(needs_live_probe(&item, 100));
    }

    #[test]
    fn has_probe_details_is_false_for_series() {
        let series = shared::model::SeriesStreamProperties::default();
        let item = item_with_props(StreamProperties::Series(Box::new(series)));
        assert!(!has_probe_details(&item));
    }

    #[test]
    fn collect_effective_skip_clusters_uses_input_skip_flags() {
        use crate::model::{ConfigInputFlags, ConfigInputOptions};
        let input = ConfigInput {
            name: "skip_live".intern(),
            input_type: InputType::Xtream,
            options: Some(ConfigInputOptions {
                flags: ConfigInputFlags::SkipLive.into(),
                ..ConfigInputOptions::defaults().clone()
            }),
            ..ConfigInput::default()
        };
        let skip = collect_effective_skip_clusters(&input);
        assert!(skip.contains(&XtreamCluster::Live));
        assert!(!skip.contains(&XtreamCluster::Video));
        assert!(!skip.contains(&XtreamCluster::Series));
    }

    #[test]
    fn filter_skipped_clusters_removes_cached_groups() {
        use crate::model::{ConfigInputFlags, ConfigInputOptions};
        let live_item = PlaylistItem {
            header: shared::model::PlaylistItemHeader {
                xtream_cluster: XtreamCluster::Live,
                ..Default::default()
            },
        };
        let vod_item = PlaylistItem {
            header: shared::model::PlaylistItemHeader {
                xtream_cluster: XtreamCluster::Video,
                ..Default::default()
            },
        };

        let groups = vec![
            PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels: vec![live_item],
                xtream_cluster: XtreamCluster::Live,
            },
            PlaylistGroup {
                id: 2,
                title: "Vod".intern(),
                channels: vec![vod_item],
                xtream_cluster: XtreamCluster::Video,
            },
        ];

        let source = MemoryPlaylistSource::new(groups).into_source();
        let input = ConfigInput {
            name: "skip_live".intern(),
            input_type: InputType::Xtream,
            options: Some(ConfigInputOptions {
                flags: ConfigInputFlags::SkipLive.into(),
                ..ConfigInputOptions::defaults().clone()
            }),
            ..ConfigInput::default()
        };

        let mut filtered = filter_skipped_clusters_from_source(source, &input);
        let filtered_groups = filtered.take_groups();
        assert_eq!(filtered_groups.len(), 1);
        assert_eq!(filtered_groups[0].xtream_cluster, XtreamCluster::Video);
    }

    fn test_group(cluster: XtreamCluster, item_name: &str, input_name: &str) -> PlaylistGroup {
        PlaylistGroup {
            id: 1,
            title: item_name.intern(),
            xtream_cluster: cluster,
            channels: vec![PlaylistItem {
                header: PlaylistItemHeader {
                    name: item_name.intern(),
                    input_name: input_name.intern(),
                    xtream_cluster: cluster,
                    item_type: match cluster {
                        XtreamCluster::Live => PlaylistItemType::Live,
                        XtreamCluster::Video => PlaylistItemType::Video,
                        XtreamCluster::Series => PlaylistItemType::Series,
                    },
                    ..Default::default()
                },
            }],
        }
    }

    #[test]
    fn staged_overlay_replaces_selected_clusters_and_rewrites_input_name() {
        let provider_name = "provider".intern();
        let provider_groups = vec![
            test_group(XtreamCluster::Live, "provider-live", "provider"),
            test_group(XtreamCluster::Video, "provider-vod", "provider"),
        ];
        let staged_groups = vec![
            test_group(XtreamCluster::Live, "staged-live", "staged"),
            test_group(XtreamCluster::Series, "staged-series", "staged"),
        ];

        let groups =
            apply_staged_overlay_groups(&provider_name, ClusterFlags::Live, provider_groups, staged_groups);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title.as_ref(), "provider-vod");
        assert_eq!(groups[0].channels[0].header.input_name.as_ref(), "provider");
        assert_eq!(groups[1].title.as_ref(), "staged-live");
        assert_eq!(groups[1].channels[0].header.input_name.as_ref(), "provider");
    }

    #[test]
    fn staged_overlay_is_skipped_when_provider_playlist_is_cached() {
        let result = PlaylistDownloadResult::new(vec![], vec![], true, false);

        assert!(!should_apply_staged_overlay(&result));
    }


    fn make_test_item(name: &str, item_type: PlaylistItemType) -> PlaylistItem {
        let header = PlaylistItemHeader {
            name: name.into(),
            group: "Test Group".intern(),
            item_type,
            ..Default::default()
        };
        PlaylistItem { header }
    }

    #[test]
    fn test_filter_evalutes_correctly() {
        let filter = get_filter(r#"name ~ "Allowed""#, None).unwrap();

        let allowed_item = make_test_item("Allowed Channel", PlaylistItemType::Live);
        let denied_item = make_test_item("Denied Channel", PlaylistItemType::Live);

        let allowed_provider = ValueProvider { pli: &allowed_item, match_as_ascii: false };
        let denied_provider = ValueProvider { pli: &denied_item, match_as_ascii: false };

        assert!(filter.filter(&allowed_provider));
        assert!(!filter.filter(&denied_provider));
    }

    #[test]
    fn test_filter_with_type_comparison() {
        let filter = get_filter("type = vod", None).unwrap();

        let vod_item = make_test_item("Test Movie", PlaylistItemType::Video);
        let live_item = make_test_item("Test Channel", PlaylistItemType::Live);

        let vod_provider = ValueProvider { pli: &vod_item, match_as_ascii: false };
        let live_provider = ValueProvider { pli: &live_item, match_as_ascii: false };

        assert!(filter.filter(&vod_provider));
        assert!(!filter.filter(&live_provider));
    }

    #[test]
    fn assign_channel_no_playlist_preserves_non_zero_chno() {
        let mut groups = vec![
            PlaylistGroup {
                id: 1,
                title: "Group A".intern(),
                channels: vec![
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "A".intern(), chno: 10, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() },
                    },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
            PlaylistGroup {
                id: 2,
                title: "Group C".intern(),
                channels: vec![
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "C".intern(), chno: 1, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "D".intern(), chno: 0, ..Default::default() },
                    },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
        ];

        assign_channel_no_playlist(&mut groups);

        // Non-zero chno values must be preserved
        assert_eq!(groups[0].channels[0].header.chno, 10);
        assert_eq!(groups[1].channels[0].header.chno, 1);
    }

    #[test]
    fn assign_channel_no_playlist_assigns_zero_chno_only() {
        let mut groups = vec![
            PlaylistGroup {
                id: 1,
                title: "Group A".intern(),
                channels: vec![
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "A".intern(), chno: 0, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "C".intern(), chno: 0, ..Default::default() },
                    },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
        ];

        assign_channel_no_playlist(&mut groups);

        // All zero-chno channels should get assigned numbers starting at 1
        assert_eq!(groups[0].channels[0].header.chno, 1);
        assert_eq!(groups[0].channels[1].header.chno, 2);
        assert_eq!(groups[0].channels[2].header.chno, 3);
    }

    #[test]
    fn assign_channel_no_playlist_skips_existing_nonzero_numbers() {
        let mut groups = vec![
            PlaylistGroup {
                id: 1,
                title: "Group A".intern(),
                channels: vec![
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "A".intern(), chno: 5, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "C".intern(), chno: 2, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "D".intern(), chno: 0, ..Default::default() },
                    },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
        ];

        assign_channel_no_playlist(&mut groups);

        // Existing non-zero numbers (2, 5) must be skipped when assigning new numbers
        assert_eq!(groups[0].channels[0].header.chno, 5); // preserved
        assert_eq!(groups[0].channels[2].header.chno, 2); // preserved
        // B gets 1 (smallest available), D gets 3 (next available after 1 and existing 2)
        assert_eq!(groups[0].channels[1].header.chno, 1);
        assert_eq!(groups[0].channels[3].header.chno, 3);
    }

    #[test]
    fn assign_channel_no_playlist_assigns_following_group_order() {
        let mut groups = vec![
            PlaylistGroup {
                id: 1,
                title: "Group 1".intern(),
                channels: vec![
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "A".intern(), chno: 0, ..Default::default() },
                    },
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "B".intern(), chno: 0, ..Default::default() },
                    },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
            PlaylistGroup {
                id: 2,
                title: "Group 2".intern(),
                channels: vec![
                    PlaylistItem {
                        header: PlaylistItemHeader { name: "C".intern(), chno: 0, ..Default::default() },
                    },
                ],
                xtream_cluster: XtreamCluster::Live,
            },
        ];

        assign_channel_no_playlist(&mut groups);

        // Numbers should follow iteration order across groups: A=1, B=2, C=3
        assert_eq!(groups[0].channels[0].header.chno, 1);
        assert_eq!(groups[0].channels[1].header.chno, 2);
        assert_eq!(groups[1].channels[0].header.chno, 3);
    }

    #[tokio::test]
    async fn parallel_input_scheduler_serializes_equal_groups_and_overlaps_distinct_groups() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        async fn observe(active: &AtomicUsize, maximum: &AtomicUsize) {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(current, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            active.fetch_sub(1, Ordering::SeqCst);
        }

        let locks = crate::utils::FileLockManager::default();
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        tokio::join!(
            with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
            with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
        );
        assert_eq!(maximum.load(Ordering::SeqCst), 1);

        maximum.store(0, Ordering::SeqCst);
        tokio::join!(
            with_sequential_group(&locks, Some(7), true, observe(&active, &maximum)),
            with_sequential_group(&locks, Some(8), true, observe(&active, &maximum)),
        );
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn parallel_input_scheduler_releases_group_after_abort() {
        let locks = Arc::new(crate::utils::FileLockManager::default());
        let task_locks = Arc::clone(&locks);
        let task = tokio::spawn(async move {
            with_sequential_group(&task_locks, Some(7), true, std::future::pending::<()>()).await;
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;

        tokio::time::timeout(
            Duration::from_secs(1),
            with_sequential_group(&locks, Some(7), true, std::future::ready(())),
        )
        .await
        .expect("aborting an input job must release its sequential group");
    }

    #[test]
    fn input_progress_message_contains_each_target_and_blocking_input() {
        let targets = ["target-a", "target-b"];
        let inputs = ["input-a", "input-b"];
        let messages: Vec<_> = targets
            .iter()
            .flat_map(|target| inputs.iter().map(move |input| target_waiting_message(target, input)))
            .collect();

        assert_eq!(messages.len(), 4);
        for target in targets {
            for input in inputs {
                assert!(messages.contains(&format!("Target '{target}' is waiting for input '{input}'")));
            }
        }
        assert!(stalker_checkpoint_message("portal-a").contains("portal-a"));
    }

    #[tokio::test]
    async fn parallel_target_pipeline_bounds_active_finalizers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut tasks = JoinSet::new();
        let mut results = Vec::new();
        let mut errors = Vec::new();

        for index in 0..6 {
            wait_for_target_finalizer_slot(&mut tasks, &mut results, &mut errors).await;
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            tasks.spawn(async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                TargetJobResult {
                    index,
                    name: format!("target-{index}"),
                    result: Ok(()),
                    errors: Vec::new(),
                }
            });
        }
        while let Some(result) = tasks.join_next().await {
            collect_target_task_result(result, &mut results, &mut errors);
        }

        assert!(maximum.load(Ordering::SeqCst) <= MAX_CONCURRENT_TARGET_FINALIZERS);
        assert_eq!(results.len(), 6);
        assert!(errors.is_empty());
    }

    #[test]
    fn parallel_target_pipeline_normalizes_conflicting_output_resources() {
        let config = Config {
            storage_dir: "/tmp/tuliprox-target-resources".to_string(),
            ..Config::default()
        };

        let mut spaced = ConfigTarget::from(&ConfigTargetDto::default());
        spaced.name = "A B".to_string();
        let mut underscored = ConfigTarget::from(&ConfigTargetDto::default());
        underscored.name = "A_B".to_string();
        assert!(!target_mutated_resources(&config, &spaced)
            .is_disjoint(&target_mutated_resources(&config, &underscored)));

        spaced.name = "one".to_string();
        spaced.output = vec![crate::model::TargetOutput::M3u(crate::model::M3uTargetOutput {
            filename: Some("out/../x.m3u".to_string()),
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        })];
        underscored.name = "two".to_string();
        underscored.output = vec![crate::model::TargetOutput::M3u(crate::model::M3uTargetOutput {
            filename: Some("x.m3u".to_string()),
            include_type_in_url: false,
            mask_redirect_url: false,
            filter: None,
        })];
        assert!(!target_mutated_resources(&config, &spaced)
            .is_disjoint(&target_mutated_resources(&config, &underscored)));
    }

    mod mapping_stage {
        use super::*;
        use crate::model::{
            EpgConfig, EpgSmartMatchConfig, IcsEpgSourceConfig, Mapper, MediaToolCapabilities, PersistedEpgSource,
            PersistedEpgSourceKind, SourcesConfig,
        };
        use crate::utils::FileLockManager;
        use arc_swap::{ArcSwap, ArcSwapOption};
        use shared::model::{ConfigPaths, EpgSmartMatchConfigDto};
        use std::sync::Arc;
        use tempfile::tempdir;
        use tokio::runtime::Runtime;

        fn build_mapping(id: &str, stage: MappingStage, script: &str) -> Mapping {
            let script = script.to_string();
            let t_filter = get_filter(r#"name ~ ".*""#, None).expect("filter parses");
            let t_script = MapperScript::parse(&script, None).expect("script parses");
            Mapping {
                id: id.to_string(),
                match_as_ascii: false,
                stage,
                mapper: Some(vec![Mapper {
                    filter: r#"name ~ ".*""#.to_string(),
                    script,
                    t_filter: Some(t_filter),
                    t_script: Some(t_script),
                }]),
                counter: None,
                t_counter: None,
                templates: None,
            }
        }

        fn build_target(mappings: Vec<Mapping>, remove_duplicates: bool) -> ConfigTarget {
            let dto = ConfigTargetDto {
                options: if remove_duplicates {
                    Some(ConfigTargetOptions { remove_duplicates, ..Default::default() })
                } else {
                    None
                },
                ..Default::default()
            };
            let mut target = ConfigTarget::from(&dto);
            target.mapping = Arc::new(ArcSwapOption::from(Some(Arc::new(mappings))));
            target
        }

        fn processing_context() -> PlaylistProcessingContext {
            let paths = ConfigPaths {
                home_path: String::new(),
                config_path: String::new(),
                storage_path: String::new(),
                config_file_path: String::new(),
                sources_file_path: String::new(),
                mapping_file_path: None,
                mapping_files_used: None,
                template_file_path: None,
                template_files_used: None,
                api_proxy_file_path: String::new(),
                custom_stream_response_path: None,
            };
            let config = AppConfig {
                config: Arc::new(ArcSwap::from_pointee(Config::default())),
                sources: Arc::new(ArcSwap::from_pointee(SourcesConfig::default())),
                hdhomerun: Arc::new(ArcSwapOption::default()),
                api_proxy: Arc::new(ArcSwapOption::default()),
                file_locks: Arc::new(FileLockManager::default()),
                paths: Arc::new(ArcSwap::from_pointee(paths)),
                custom_stream_response: Arc::new(ArcSwapOption::default()),
                access_token_secret: [0; 32],
                encrypt_secret: [0; 16],
                media_tools: Arc::new(MediaToolCapabilities::new()),
            };
            PlaylistProcessingContext {
                client: reqwest::Client::new(),
                config: Arc::new(config),
                user_targets: Arc::new(ProcessTargets {
                    enabled: false,
                    inputs: Vec::new(),
                    targets: Vec::new(),
                    target_names: Vec::new(),
                }),
                event_manager: None,
                playlist_state: None,
                disabled_headers: None,
                processed_inputs: Arc::new(Mutex::new(HashSet::new())),
                input_locks: Arc::new(Mutex::new(HashMap::new())),
                provider_manager: None,
                metadata_manager: None,
                pre_processed_inputs: None,
                stalker_refresh_mode: StalkerRefreshMode::Complete,
                partial_refresh: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }

        fn make_channel(name: &str) -> PlaylistItem {
            let mut item = PlaylistItem {
                header: PlaylistItemHeader {
                    name: name.intern(),
                    group: "Originals".intern(),
                    xtream_cluster: XtreamCluster::Live,
                    item_type: PlaylistItemType::Live,
                    ..Default::default()
                },
            };
            item.header.freeze_input_stream_id();
            item
        }

        fn memory_source(channels: Vec<PlaylistItem>) -> PlaylistSource {
            MemoryPlaylistSource::new(vec![PlaylistGroup {
                id: 1,
                title: "Live".intern(),
                channels,
                xtream_cluster: XtreamCluster::Live,
            }])
            .into_source()
        }

        fn channel_count(source: &mut PlaylistSource) -> usize {
            source
                .take_groups()
                .iter()
                .map(|g| g.channels.len())
                .sum()
        }

        #[test]
        fn map_playlist_applies_only_the_requested_stage() {
            let processing = build_mapping(
                "processing",
                MappingStage::Processing,
                r#"@name = concat(@Name, "-P")"#,
            );
            let after_epg = build_mapping(
                "after_epg",
                MappingStage::AfterEpg,
                r#"@name = concat(@Name, "-E")"#,
            );
            let target = build_target(vec![processing, after_epg], false);

            let mut source = memory_source(vec![make_channel("Alpha")]);
            let groups = map_playlist(&mut source, &target).expect("processing mapping should run");
            assert_eq!(groups[0].channels[0].header.name.as_ref(), "Alpha-P");

            let mut source = MemoryPlaylistSource::new(groups).into_source();
            let groups = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None)
                .expect("after_epg mapping should run");
            assert_eq!(groups[0].channels[0].header.name.as_ref(), "Alpha-P-E");
        }

        #[test]
        fn map_playlist_at_stage_returns_none_without_consuming_source_when_no_match() {
            let target = build_target(Vec::new(), false);
            let mut source = memory_source(vec![make_channel("Alpha")]);

            let result = map_playlist_at_stage(&mut source, &target, MappingStage::AfterEpg, None);
            assert!(result.is_none(), "no matching stage must return None");
            assert_eq!(channel_count(&mut source), 1, "source must remain intact");
        }

        #[test]
        fn prepare_target_applies_after_epg_mapping_before_sampling_stats() {
            let runtime = Runtime::new().expect("runtime");
            runtime.block_on(async {
                let dir = tempdir().expect("tempdir");
                let ics_path = dir.path().join("bbc.ics");
                std::fs::write(
                    &ics_path,
                    "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:News\nDTSTART:20260306T120000Z\nDTEND:20260306T130000Z\nEND:VEVENT\nEND:VCALENDAR",
                )
                .expect("write ics");

                let mut smart_dto = EpgSmartMatchConfigDto {
                    enabled: true,
                    fuzzy_matching: false,
                    ..EpgSmartMatchConfigDto::default()
                };
                smart_dto.prepare().expect("smart config");
                let mut input = ConfigInput::from(ConfigInputDto::default());
                input.name = "input".intern();
                input.epg = Some(EpgConfig {
                    sources: vec![],
                    smart_match: Some(EpgSmartMatchConfig::from(smart_dto)),
                });

                let channels = vec![live_item_for_epg("BBC One")];
                let groups = vec![PlaylistGroup {
                    id: 1,
                    title: "Live".intern(),
                    channels,
                    xtream_cluster: XtreamCluster::Live,
                }];
                let tv_guide = TVGuide::new(vec![PersistedEpgSource {
                    file_path: ics_path,
                    priority: 0,
                    logo_override: false,
                    kind: PersistedEpgSourceKind::Ics {
                        channel_id: "bbc.one".intern(),
                        channel_title: Some("BBC One".intern()),
                        match_names: vec!["BBC One".intern()],
                        config: Box::new(IcsEpgSourceConfig::default()),
                    },
                }]);

                let mut playlist = FetchedPlaylist {
                    input: &input,
                    source: MemoryPlaylistSource::new(groups).into_source(),
                    epg: Some(tv_guide),
                };

                let rename_from_epg = build_mapping(
                    "rename",
                    MappingStage::AfterEpg,
                    r#"epg = @epg_channel_id ~ "(.+)"
match {
  epg => @Name = epg.1
}"#,
                );
                let add_virtual = build_mapping("virtual", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let target = build_target(vec![rename_from_epg, add_virtual], false);
                let mut stats = HashMap::from([(
                    Arc::clone(&input.name),
                    create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
                )]);
                let mut errors = Vec::new();
                let prepared = prepare_playlist_for_target(
                    &processing_context(),
                    std::slice::from_mut(&mut playlist),
                    &target,
                    &mut stats,
                    &mut errors,
                    false,
                )
                .await
                .expect("target preparation");

                assert!(errors.is_empty());
                assert_eq!(prepared.playlist.iter().map(|group| group.channels.len()).sum::<usize>(), 2);
                let channel = prepared
                    .playlist
                    .iter()
                    .flat_map(|group| &group.channels)
                    .find(|channel| channel.header.group.as_ref() != "Echo")
                    .expect("original channel");
                assert_eq!(channel.header.epg_channel_id.as_deref(), Some("bbc.one"));
                assert_eq!(
                    channel.header.name.as_ref(),
                    "bbc.one",
                    "after_epg mapper must consume the EPG-enriched field"
                );
                let processed_stats = &stats[&input.name].processed_stats;
                assert_eq!(processed_stats.group_count, 2);
                assert_eq!(processed_stats.channel_count, 2);
            });
        }

        fn live_item_for_epg(name: &str) -> PlaylistItem {
            PlaylistItem {
                header: PlaylistItemHeader {
                    name: name.intern(),
                    group: "Live".intern(),
                    xtream_cluster: XtreamCluster::Live,
                    item_type: PlaylistItemType::Live,
                    ..Default::default()
                },
            }
        }

        #[test]
        fn after_epg_hook_runs_on_source_already_deduplicated_by_processing_pipe() {
            let processing = build_mapping(
                "processing",
                MappingStage::Processing,
                r#"@group = "PROCESSED""#,
            );
            let after_epg = build_mapping("after_epg", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
            let target = build_target(vec![processing, after_epg], true);

            let input = ConfigInput::default();
            let channel = make_channel("Alpha");
            let mut fetched = FetchedPlaylist {
                input: &input,
                source: memory_source(vec![channel.clone(), channel]),
                epg: None,
            };
            let mut duplicates = HashSet::new();
            let mut processed = execute_pipe(
                &target,
                &get_processing_pipe(&target),
                &mut fetched,
                &mut duplicates,
                false,
            )
            .expect("processing pipe must run");
            assert_eq!(processed.get_channel_count(), 1, "processing pipe must remove the duplicate");

            let groups = map_playlist_at_stage(&mut processed.source, &target, MappingStage::AfterEpg, None)
                .expect("after_epg hook must run");

            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0].title.as_ref(), "PROCESSED");
            assert_eq!(groups[0].channels.len(), 1);
            assert_eq!(groups[1].title.as_ref(), "Echo");
            assert_eq!(groups[1].channels.len(), 1);
        }

        #[test]
        fn prepare_target_deduplicates_virtual_items_created_by_after_epg_mappings() {
            let runtime = Runtime::new().expect("runtime");
            runtime.block_on(async {
                let first = build_mapping("first", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let second = build_mapping("second", MappingStage::AfterEpg, r#"add_favourite("Echo")"#);
                let target = build_target(vec![first, second], true);
                let input = ConfigInput { name: "input".intern(), ..Default::default() };
                let mut playlist = FetchedPlaylist {
                    input: &input,
                    source: memory_source(vec![make_channel("Alpha")]),
                    epg: None,
                };
                let mut stats = HashMap::from([(
                    Arc::clone(&input.name),
                    create_input_stat(1, 1, 0, input.input_type, &input.name, 0),
                )]);
                let mut errors = Vec::new();

                let prepared = prepare_playlist_for_target(
                    &processing_context(),
                    std::slice::from_mut(&mut playlist),
                    &target,
                    &mut stats,
                    &mut errors,
                    false,
                )
                .await
                .expect("target preparation");

                assert!(errors.is_empty());
                assert_eq!(prepared.playlist.iter().map(|group| group.channels.len()).sum::<usize>(), 3);
                assert_eq!(stats[&input.name].processed_stats.channel_count, 3);
            });
        }
    }
}

#[cfg(test)]
mod disk_epg_wireup_tests {
    use super::spill_epg_to_disk;
    use crate::model::Epg;
    use shared::model::EpgChannel;
    use std::sync::Arc;

    /// Build an `Epg` with `channel_count` channels whose ids follow the
    /// `id_base` prefix. Two sources built with the same `id_base` and
    /// `channel_count` will share all channel ids, which is what we need to
    /// exercise the priority-override `Occupied` branch in
    /// `EpgMergeAccumulator::upsert_channel`.
    fn build_epg(id_base: &str, priority: i16, channel_count: usize) -> Epg {
        Epg {
            priority,
            logo_override: false,
            attributes: None,
            children: (0..channel_count)
                .map(|i| {
                    let id: Arc<str> = format!("{id_base}-ch-{i:04}").into();
                    Arc::new(EpgChannel {
                        id: Arc::clone(&id),
                        title: Some(format!("title-{priority}-{i}").into()),
                        icon: None,
                        programmes: vec![shared::model::EpgProgramme::new(
                            i64::try_from(i).expect("test index fits in i64"),
                            i64::try_from(i + 1).expect("test index fits in i64"),
                            id,
                        )],
                    })
                })
                .collect(),
        }
    }

    /// Wire-up regression guard: `spill_epg_to_disk` is the function called
    /// by `finalize_prepared_target` when `disk_based_processing = true`. It
    /// must (a) preserve per-source priority on shared channels, (b) clean up
    /// its temp files, and (c) merge into a single `Epg` of the right size.
    ///
    /// Both sources share channel ids (`shared-ch-NNNN`), forcing the merge
    /// to take the `Occupied` branch in `EpgMergeAccumulator::upsert_channel`.
    /// The lower-priority source (priority 3) must win, the higher-priority
    /// (priority 7) must be discarded for shared ids. Without this assertion
    /// the test would pass even if priority resolution were broken — the
    /// earlier version used unique ids and therefore never hit the merge path.
    #[test]
    fn spill_epg_to_disk_merges_shared_channels_by_priority() {
        let epg_low = build_epg("shared", 3, 50); // wins on every shared channel
        let epg_high = build_epg("shared", 7, 50); // discarded on every shared channel

        let merged = spill_epg_to_disk(vec![epg_low, epg_high])
            .expect("disk merge returned an error")
            .expect("merged Epg is unexpectedly None for two non-empty sources");

        // 50 distinct channels, not 100 — the merge must have collapsed the
        // shared ids.
        assert_eq!(
            merged.children.len(),
            50,
            "shared channel ids must collapse to one entry, not be duplicated"
        );

        // Every channel title comes from the lower-priority source. If the
        // merge logic is wrong, some titles will carry the "-7-" marker.
        for ch in &merged.children {
            let title = ch.title.as_deref().expect("title preserved through merge");
            assert!(
                title.starts_with("title-3-"),
                "channel {:?} kept title {title:?} from higher-priority source; \
                 priority override is broken",
                ch.id,
            );
            // `add_channel_with_programmes` on the disk-merge path must
            // preserve the lower-priority source's single programme per
            // channel — `upsert_channel` would silently drop them.
            assert_eq!(
                ch.programmes.len(),
                1,
                "channel {:?} lost programmes through the disk-merge path",
                ch.id
            );
            let prog = &ch.programmes[0];
            assert!(prog.title.is_none() || prog.title.as_deref() != Some("title-7"));
        }
    }

    /// The non-shared case: sources with disjoint channel ids. Both
    /// sources' channels appear in the result with no priority loss (no
    /// `Occupied` branch is taken).
    #[test]
    fn spill_epg_to_disk_keeps_disjoint_sources_intact() {
        let epg_low = build_epg("src-a", 3, 50);
        let epg_high = build_epg("src-b", 7, 50);

        let merged = spill_epg_to_disk(vec![epg_low, epg_high])
            .expect("disk merge returned an error")
            .expect("merged Epg is unexpectedly None for two non-empty sources");

        assert_eq!(merged.children.len(), 100, "disjoint ids must not collapse");
        assert!(merged.children.iter().any(|ch| ch.title.as_deref() == Some("title-3-0")));
        assert!(merged.children.iter().any(|ch| ch.title.as_deref() == Some("title-7-0")));
    }

    #[test]
    fn spill_epg_to_disk_returns_none_for_empty_input() {
        let merged = spill_epg_to_disk(vec![]).expect("disk merge returned an error");
        assert!(merged.is_none());
    }
}
