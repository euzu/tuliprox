use crate::{
    api::{
        model::{
            ActiveProviderManager, AppState, EventManager, EventMessage, MetadataUpdateManager, PlaylistStorageState,
            ProviderIdType, ResolveReason, UpdateGuard, UpdateTask,
        },
        sync_panel_api_exp_dates,
    },
    messaging::send_message,
    model::{
        AppConfig, ConfigFavourites, ConfigInput, ConfigInputFlags, ConfigInputOptions, ConfigRename, ConfigTarget,
        FetchedPlaylist, Mapping, MessageContent, ProcessTargets, ReverseProxyDisabledHeaderConfig, TVGuide,
    },
    processing::{
        input_cache,
        input_cache::ClusterState,
        parser::xmltv::flatten_tvguide,
        playlist_watch::process_group_watch,
        processor::{
            epg::process_playlist_epg, library, sort::sort_playlist, trakt::process_trakt_categories_for_target,
            xtream_series::playlist_resolve_series, xtream_vod::playlist_resolve_vod,
        },
    },
    repository::{
        load_input_playlist, persist_input_playlist, persist_playlist, CategoryKey, MemoryPlaylistSource,
        PlaylistSource,
    },
    utils::{
        debug_if_enabled, epg, log_memory_snapshot, m3u, trace_if_enabled, xtream, StepMeasure, StepMeasureCallback,
    },
};
use super::filtered_playlist_source::FilteredPlaylistSource;
use futures::{FutureExt, StreamExt};
use indexmap::IndexMap;
use log::{debug, error, info, log_enabled, warn, Level};
use shared::{
    concat_string,
    error::{get_errors_notify_message, notify_err, TuliproxError},
    foundation::{get_field_value, set_field_value, Filter, ValueAccessor, ValueProvider},
    model::{
        ClusterSource, CounterModifier, FieldGetAccessor, FieldSetAccessor, InputStats, InputType, ItemField,
        PlaylistGroup, PlaylistItem, PlaylistItemType, PlaylistStats, ProcessingOrder, SourceStats, StreamProperties,
        TargetStats, UUIDType, XtreamCluster,
    },
    utils::{
        create_alias_uuid, default_as_default, default_probe_delay_secs, default_probe_live_interval, interner_gc,
        Internable,
    },
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex, OwnedRwLockWriteGuard, RwLock},
    task::JoinSet,
};

const PLAYLIST_UPDATE_MAX_DURATION_SECS: u64 = 3600;

fn is_valid(pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool) -> bool {
    let provider = ValueProvider { pli, match_as_ascii };
    filter.filter(&provider)
}

pub fn apply_filter_to_source(source: &mut dyn PlaylistSource, filter: &Filter) -> Option<Vec<PlaylistGroup>> {
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

fn filter_playlist(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    apply_filter_to_source(source, &target.filter)
}

pub fn apply_filter_to_playlist(playlist: &mut [PlaylistGroup], filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    let mut new_playlist = Vec::with_capacity(128);
    for pg in playlist.iter_mut() {
        let channels =
            pg.channels.iter().filter(|&pli| is_valid(pli, filter, false)).cloned().collect::<Vec<PlaylistItem>>();
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
                let value = cap.into_owned();
                set_field_value(result, r.field, value);
            }
        }
    }
}

fn rename_playlist(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
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

fn map_playlist(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    let mapping_binding = target.mapping.load();
    let mappings = mapping_binding.as_ref()?;
    let valid_mappings = mappings.iter().filter(|m| m.mapper.as_ref().is_some_and(|v| !v.is_empty()));
    let iter: Box<dyn Iterator<Item = PlaylistItem>> = Box::new(source.into_items());
    let mapped_iter = valid_mappings.fold(iter, |iter, mapping| {
        Box::new(iter.flat_map(move |chan| map_channel_and_flatten(chan, mapping)))
            as Box<dyn Iterator<Item = PlaylistItem>>
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

    Some(next_groups.into_values().collect())
}

fn map_playlist_counter(target: &ConfigTarget, playlist: &mut [PlaylistGroup]) {
    if let Some(guard) = &*target.mapping.load() {
        let mappings = guard.as_ref();
        for mapping in mappings {
            if let Some(counter_list) = &mapping.t_counter {
                for counter in counter_list {
                    for plg in &mut *playlist {
                        for channel in &mut plg.channels {
                            let provider = ValueProvider { pli: channel, match_as_ascii: mapping.match_as_ascii };
                            if counter.filter.filter(&provider) {
                                let cntval = counter.value.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
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

struct PlaylistDownloadResult {
    pub downloaded_playlist: Vec<PlaylistGroup>,
    pub download_err: Vec<TuliproxError>,
    pub was_cached: bool,
    pub persisted: bool,
}

impl PlaylistDownloadResult {
    pub fn new(
        downloaded_playlist: Vec<PlaylistGroup>,
        download_err: Vec<TuliproxError>,
        was_cached: bool,
        persisted: bool,
    ) -> Self {
        Self { downloaded_playlist, download_err, was_cached, persisted }
    }
}

/// Returns true when the input uses a hybrid download strategy:
/// M3U staged providing some clusters and Xtream main providing others.
fn is_hybrid_m3u_xtream(input: &ConfigInput) -> bool {
    if !input.input_type.is_xtream() {
        return false;
    }
    input.staged.as_ref().is_some_and(|s| s.enabled && s.input_type.is_m3u())
}

fn collect_effective_skip_clusters(input: &ConfigInput) -> Vec<XtreamCluster> {
    if !input.input_type.is_xtream() {
        return vec![];
    }

    let mut skip_cluster = xtream::get_skip_cluster(input);
    if let Some(staged) = input.staged.as_ref().filter(|staged| staged.enabled) {
        for cluster in [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series] {
            if staged.get_cluster_source(cluster) == ClusterSource::Skip && !skip_cluster.contains(&cluster) {
                skip_cluster.push(cluster);
            }
        }
    }
    skip_cluster
}

fn hybrid_has_m3u_staged_cluster(input: &ConfigInput, skip_cluster: &[XtreamCluster]) -> bool {
    if !is_hybrid_m3u_xtream(input) {
        return false;
    }

    input.staged.as_ref().is_some_and(|staged| {
        [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series]
            .iter()
            .any(|cluster| {
                !skip_cluster.contains(cluster)
                    && staged.get_cluster_source(*cluster) == ClusterSource::Staged
            })
    })
}

fn filter_skipped_clusters_from_source(
    source: Box<dyn PlaylistSource>,
    input: &ConfigInput,
) -> Box<dyn PlaylistSource> {
    let skip_clusters = collect_effective_skip_clusters(input);
    if skip_clusters.is_empty() {
        return source;
    }

    let skip_set: HashSet<XtreamCluster> = skip_clusters.into_iter().collect();
    Box::new(FilteredPlaylistSource::new(source, skip_set))
}

#[allow(clippy::too_many_lines)]
async fn playlist_download_from_input(
    client: &reqwest::Client,
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
) -> PlaylistDownloadResult {
    let config = &*app_config.config.load();
    let storage_dir = &config.storage_dir;

    // Check Status
    let storage_path = input_cache::resolve_input_storage_path(storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&storage_path);
    let cache_duration = input.cache_duration_seconds;

    // Ensure data directory exists
    if !storage_path.exists() {
        let _ = std::fs::create_dir_all(&storage_path);
    }

    let hybrid = is_hybrid_m3u_xtream(input);
    let download_input_type = input.get_download_input_type();
    // Use per-cluster cache for effective Xtream downloads and hybrid M3U+Xtream inputs.
    let use_per_cluster_cache = hybrid || download_input_type.is_xtream();

    let mut xtream_clusters_to_download = Vec::new();
    let mut needs_m3u_download = false;
    let fully_cached = if use_per_cluster_cache {
        let skip_cluster = collect_effective_skip_clusters(input);
        let (staged_candidates, main_candidates) =
            xtream::partition_clusters_by_source(input, None, &skip_cluster);

        let mut xtream_cache_candidates = staged_candidates;
        xtream_cache_candidates.extend(main_candidates);

        for cluster in xtream_cache_candidates {
            if !input_cache::is_cache_valid(&status, &cluster.to_string(), cache_duration) {
                xtream_clusters_to_download.push(cluster);
            }
        }

        if hybrid {
            if hybrid_has_m3u_staged_cluster(input, &skip_cluster) {
                needs_m3u_download = !input_cache::is_cache_valid(&status, "default", cache_duration);
            }
            xtream_clusters_to_download.is_empty() && !needs_m3u_download
        } else {
            xtream_clusters_to_download.is_empty()
        }
    } else {
        input_cache::is_cache_valid(&status, "default", cache_duration)
    };

    if fully_cached {
        return PlaylistDownloadResult::new(vec![], vec![], true, false);
    }

    let (playlist, errors, persisted, m3u_error_count, xtream_error_count) = if hybrid {
        let mut playlist = Vec::new();
        let mut all_errors = Vec::new();
        let mut m3u_error_count = 0usize;
        let mut xtream_error_count = 0usize;
        let mut m3u_added_groups = false;

        if needs_m3u_download {
            let staged = input.staged.as_ref().expect("hybrid requires staged input");
            let staged_source: crate::model::InputSource = staged.into();
            let (m3u_playlist, m3u_errors) =
                m3u::download_m3u_playlist_from_source(app_config, client, config, input, Some(staged_source)).await;
            m3u_error_count = m3u_errors.len();
            m3u_added_groups = !m3u_playlist.is_empty();
            playlist.extend(m3u_playlist);
            all_errors.extend(m3u_errors);
        }

        let mut xtream_persisted = false;
        if !xtream_clusters_to_download.is_empty() {
            let (xtream_playlist, xtream_errors, persisted) = xtream::download_xtream_playlist(
                app_config,
                client,
                input,
                Some(xtream_clusters_to_download.as_slice()),
            )
            .await;
            xtream_error_count = xtream_errors.len();
            playlist.extend(xtream_playlist);
            all_errors.extend(xtream_errors);
            xtream_persisted = persisted;

            if m3u_added_groups {
                // Keep merged hybrid output in memory when staged M3U contributed groups.
                xtream_persisted = false;
            }
        }

        (playlist, all_errors, xtream_persisted, m3u_error_count, xtream_error_count)
    } else {
        match download_input_type {
            InputType::M3u => {
                let (p, e) = m3u::download_m3u_playlist(app_config, client, config, input).await;
                (p, e, false, 0, 0)
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
                (p, e, persisted, 0, xtream_error_count)
            }
            InputType::M3uBatch | InputType::XtreamBatch => (vec![], vec![], false, 0, 0),
            InputType::Library => {
                let (p, e) = library::download_library_playlist(client, app_config, input).await;
                (p, e, false, 0, 0)
            }
        }
    };

    // Update Status
    let mut save_status = false;
    if hybrid {
        if needs_m3u_download {
            let m3u_state = if m3u_error_count == 0 { ClusterState::Ok } else { ClusterState::Failed };
            input_cache::update_cluster_status(&mut status, "default", m3u_state);
            save_status = true;
        }

        if !xtream_clusters_to_download.is_empty() {
            let state = if xtream_error_count == 0 { ClusterState::Ok } else { ClusterState::Failed };
            for cluster in &xtream_clusters_to_download {
                input_cache::update_cluster_status(&mut status, &cluster.to_string(), state.clone());
            }
            save_status = true;
        }
    } else if errors.is_empty() {
        if use_per_cluster_cache {
            for cluster in &xtream_clusters_to_download {
                input_cache::update_cluster_status(&mut status, &cluster.to_string(), ClusterState::Ok);
            }
            save_status = !xtream_clusters_to_download.is_empty();
        } else {
            input_cache::update_cluster_status(&mut status, "default", ClusterState::Ok);
            save_status = true;
        }
    } else if use_per_cluster_cache {
        for cluster in &xtream_clusters_to_download {
            input_cache::update_cluster_status(&mut status, &cluster.to_string(), ClusterState::Failed);
        }
        save_status = !xtream_clusters_to_download.is_empty();
    } else {
        input_cache::update_cluster_status(&mut status, "default", ClusterState::Failed);
        save_status = true;
    }

    if save_status {
        input_cache::save_input_status(&storage_path, &status);
    }

    PlaylistDownloadResult::new(playlist, errors, false, persisted)
}

#[allow(clippy::too_many_lines)]
async fn process_source(
    source_idx: usize,
    ctx: &PlaylistProcessingContext,
) -> (Vec<InputStats>, Vec<TargetStats>, Vec<TuliproxError>) {
    log_memory_snapshot(format!("source[{source_idx}] start").as_str());
    let sources = ctx.config.sources.load();
    let mut errors = vec![];
    let mut input_stats = HashMap::<Arc<str>, InputStats>::new();
    let mut target_stats = Vec::<TargetStats>::new();
    if let Some(source) = sources.get_source_at(source_idx) {
        let mut source_playlists = Vec::with_capacity(source.inputs.len());
        let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());
        // Download the sources
        let mut source_downloaded = false;
        let mut disabled_inputs: Vec<Arc<str>> = vec![];
        for input_name in &source.inputs {
            let Some(input) = sources.get_input_by_name(input_name) else {
                error!("Input {input_name} referenced by source {source_idx} does not exist");
                continue;
            };
            if is_input_enabled(input, &ctx.user_targets) {
                let effective_input_type = input.get_download_input_type();
                source_downloaded = true;
                log_memory_snapshot(format!("source[{source_idx}] input '{}' before_download", input.name).as_str());

                let start_time = Instant::now();
                // Download the playlist for input
                let (mut playlist_groups, mut error_list) = {
                    broadcast_step("Playlist download", &format!("Downloading input '{}'", input.name));

                    let (mut download_err, playlist, error) = download_input(ctx, input).await;

                    if let Some(err) = error {
                        broadcast_step(
                            "Playlist download",
                            &format!("Failed to persist/load input '{}' playlist", input.name),
                        );
                        error!("Failed to persist input playlist {}", input.name);
                        download_err.push(err);
                    }
                    (playlist, download_err)
                };
                log_memory_snapshot(format!("source[{source_idx}] input '{}' after_download", input.name).as_str());

                let tvguide = if effective_input_type == InputType::Library {
                    None
                } else {
                    download_input_epg(ctx, input, &mut error_list).await
                };
                log_memory_snapshot(format!("source[{source_idx}] input '{}' after_epg_download", input.name).as_str());

                errors.append(&mut error_list);
                let group_count = playlist_groups.get_group_count();
                let channel_count = playlist_groups.get_channel_count();
                let input_name = &input.name;
                if playlist_groups.is_empty() {
                    broadcast_step("Playlist download", &format!("Input '{}' playlist is empty", input.name));
                    info!("Source is empty {input_name}");
                    errors.push(notify_err!("Source is empty {input_name}"));
                } else {
                    source_playlists.push(FetchedPlaylist { input, source: playlist_groups, epg: tvguide });
                    log_memory_snapshot(
                        format!("source[{source_idx}] input '{}' after_source_push", input.name).as_str(),
                    );
                }
                let elapsed = start_time.elapsed().as_secs();
                input_stats.insert(
                    input_name.clone(),
                    create_input_stat(group_count, channel_count, errors.len(), effective_input_type, input_name, elapsed),
                );
            } else {
                disabled_inputs.push(input.name.clone());
            }
        }
        if !disabled_inputs.is_empty() && !source_downloaded {
            warn!(
                "Source at index {source_idx} has no enabled inputs for the given targets. Disabled: {}",
                disabled_inputs.iter().map(std::convert::AsRef::as_ref).collect::<Vec<&str>>().join(", ")
            );
        }
        if source_downloaded {
            if source_playlists.is_empty() {
                debug!("Source at index {source_idx} is empty");
                errors.push(notify_err!(
                    "Source at index {source_idx} is empty: {}",
                    source.inputs.iter().map(Clone::clone).collect::<Vec<Arc<str>>>().join(", ")
                ));
            } else {
                debug_if_enabled!(
                    "Source has {} groups",
                    source_playlists.iter_mut().map(FetchedPlaylist::get_channel_count).sum::<usize>()
                );
                let enabled_targets: Vec<_> =
                    source.targets.iter().filter(|target| is_target_enabled(target, &ctx.user_targets)).collect();

                for (idx, target) in enabled_targets.iter().enumerate() {
                    let consume_input_source = idx + 1 == enabled_targets.len();
                    debug!(
                        "Processing target '{}' (use_memory_cache={}, consume_input_source={})",
                        target.name, target.use_memory_cache, consume_input_source
                    );
                    log_memory_snapshot(
                        format!("source[{source_idx}] target '{}' before_process", target.name).as_str(),
                    );
                    match process_playlist_for_target(
                        ctx,
                        &mut source_playlists,
                        target,
                        &mut input_stats,
                        &mut errors,
                        consume_input_source,
                    )
                    .await
                    {
                        Ok(()) => {
                            target_stats.push(TargetStats::success(&target.name));
                        }
                        Err(mut err) => {
                            target_stats.push(TargetStats::failure(&target.name));
                            errors.append(&mut err);
                        }
                    }
                    log_memory_snapshot(
                        format!("source[{source_idx}] target '{}' after_process", target.name).as_str(),
                    );
                }
            }
        }
    }
    log_memory_snapshot(format!("source[{source_idx}] end").as_str());
    (input_stats.into_values().collect(), target_stats, errors)
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

async fn invalidate_input_cache_status(ctx: &PlaylistProcessingContext, input: &ConfigInput) {
    let storage_dir = { ctx.config.config.load().storage_dir.clone() };
    let storage_path = input_cache::resolve_input_storage_path(&storage_dir, &input.name).await;
    let mut status = input_cache::load_input_status(&storage_path);
    if !status.clusters.is_empty() {
        status.clusters.clear();
        input_cache::save_input_status(&storage_path, &status);
    }
}

async fn download_input(
    ctx: &PlaylistProcessingContext,
    input: &Arc<ConfigInput>,
) -> (Vec<TuliproxError>, Box<dyn PlaylistSource>, Option<TuliproxError>) {
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
            playlist_download_from_input(&ctx.client, &ctx.config, input).await
        }
    } else {
        PlaylistDownloadResult::new(vec![], vec![], true, false)
    };

    let mut preloaded_playlist: Option<(Box<dyn PlaylistSource>, Option<TuliproxError>)> = None;
    if playlist_download_result.was_cached {
        let (cached_playlist, cached_error) = match load_input_playlist(ctx, input, None).await {
            Ok(pl_source) => (pl_source, None),
            Err(e) => (MemoryPlaylistSource::default().boxed(), Some(e)),
        };
        // Defensive fallback: if cache metadata says "valid" but persisted data is unreadable,
        // retry once before forcing a refresh.
        let must_force_refresh = cached_error.is_some();
        if must_force_refresh {
            warn!(
                "Input '{}' cache hit produced unreadable playlist; retrying cached load once",
                input.name
            );
            if let Ok(retry_playlist) = load_input_playlist(ctx, input, None).await {
                preloaded_playlist = Some((retry_playlist, None));
            } else {
                if input_lock.is_none() {
                    input_lock = Some(ctx.get_input_lock(&input.name).await);
                }
                warn!(
                    "Input '{}' cached playlist remained unreadable after retry; invalidating cache and forcing refresh",
                    input.name
                );
                invalidate_input_cache_status(ctx, input).await;
                playlist_download_result = playlist_download_from_input(&ctx.client, &ctx.config, input).await;
            }
        } else {
            preloaded_playlist = Some((cached_playlist, None));
        }
    }

    let (playlist, error) = if let Some(preloaded) = preloaded_playlist {
        preloaded
    } else if playlist_download_result.was_cached || playlist_download_result.persisted {
        match load_input_playlist(ctx, input, None).await {
            Ok(pl_source) => (pl_source, None),
            Err(e) => (MemoryPlaylistSource::default().boxed(), Some(e)),
        }
    } else {
        debug!("Persisting input '{}' playlist", input.name);
        let (pl, err) = persist_input_playlist(&ctx.config, input, playlist_download_result.downloaded_playlist).await;
        (MemoryPlaylistSource::new(pl).boxed(), err)
    };

    let playlist = filter_skipped_clusters_from_source(playlist, input);

    if mark_as_processed {
        // Mark after persist/load so other workers only see this input as ready when data is usable.
        ctx.mark_input_downloaded(input.name.clone()).await;
    }

    // Explicitly release per-input lock after load/persist/mark steps are completed.
    drop(input_lock);

    (playlist_download_result.download_err, playlist, error)
}

fn create_broadcast_callback(event_manager: Option<&Arc<EventManager>>) -> StepMeasureCallback {
    if let Some(event_mgr) = event_manager {
        let events = event_mgr.clone();
        Box::new(move |context: &str, msg: &str| {
            events.send_event(EventMessage::PlaylistUpdateProgress(context.to_owned(), msg.to_owned()));
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
    let process_parallel = processing_ctx.config.config.load().process_parallel && sources.sources.len() > 1;
    if process_parallel && log_enabled!(Level::Debug) {
        debug!("Parallel processing enabled");
    }

    let errors = Arc::new(Mutex::<Vec<TuliproxError>>::new(vec![]));
    let stats = Arc::new(Mutex::<Vec<SourceStats>>::new(vec![]));
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

        let shared_errors = errors.clone();
        let shared_stats = stats.clone();
        let ctx = processing_ctx.clone();

        processed_any = true;
        if process_parallel {
            async_tasks.spawn(async move {
                // Hold the per-source lock for the full duration of this update.
                let current_update_lock = update_lock;
                let (input_stats, target_stats, mut res_errors) = process_source(index, &ctx).await;
                shared_errors.lock().await.append(&mut res_errors);
                if let Some(process_stats) = SourceStats::try_new(input_stats, target_stats) {
                    shared_stats.lock().await.push(process_stats);
                }
                drop(current_update_lock);
            });
        } else {
            let (input_stats, target_stats, mut res_errors) = process_source(index, &ctx).await;
            shared_errors.lock().await.append(&mut res_errors);
            if let Some(process_stats) = SourceStats::try_new(input_stats, target_stats) {
                shared_stats.lock().await.push(process_stats);
            }
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
        if let Err(err) = result {
            error!("Playlist processing task failed: {err:?}");
        }
    }
    if let (Ok(s), Ok(e)) = (Arc::try_unwrap(stats), Arc::try_unwrap(errors)) {
        (s.into_inner(), e.into_inner())
    } else {
        (vec![], vec![])
    }
}

pub type ProcessingPipe = Vec<fn(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>>>;

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
) -> FetchedPlaylist<'a> {
    let source = if consume_source {
        if fpl.is_memory() {
            MemoryPlaylistSource::new(fpl.source.take_groups()).boxed()
        } else {
            std::mem::replace(&mut fpl.source, MemoryPlaylistSource::default().boxed())
        }
    } else {
        fpl.clone_source()
    };

    let mut new_fpl = FetchedPlaylist { input: fpl.input, source, epg: fpl.epg.clone() };
    if target.options.as_ref().is_some_and(|opt| opt.remove_duplicates) {
        new_fpl.deduplicate(duplicates);
    }

    for f in pipe {
        if let Some(groups) = f(new_fpl.source.as_mut(), target) {
            new_fpl.source = MemoryPlaylistSource::new(groups).boxed();
        }
    }
    // Ensure source is memory-based for downstream mutable processing (VOD/series resolution)
    if !new_fpl.is_memory() {
        new_fpl.source = MemoryPlaylistSource::new(new_fpl.source.take_groups()).boxed();
    }
    new_fpl
}

// This method is needed, because of duplicate group names in different inputs.
// We merge the same group names considering cluster together.
fn flatten_groups(playlistgroups: Vec<PlaylistGroup>) -> Vec<PlaylistGroup> {
    let mut sort_order: Vec<PlaylistGroup> = vec![];
    let mut idx: usize = 0;
    let mut group_map: HashMap<CategoryKey, usize> = HashMap::new();
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

#[allow(clippy::too_many_arguments)]
async fn process_playlist_for_target(
    ctx: &PlaylistProcessingContext,
    playlists: &mut [FetchedPlaylist<'_>],
    target: &ConfigTarget,
    stats: &mut HashMap<Arc<str>, InputStats>,
    errors: &mut Vec<TuliproxError>,
    consume_input_source: bool,
) -> Result<(), Vec<TuliproxError>> {
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
        let mut processed_fpl = execute_pipe(target, &pipe, provider_fpl, &mut duplicates, consume_input_source);
        log_memory_snapshot(
            format!("target '{}' input '{}' after_pipe", target.name, provider_fpl.input.name).as_str(),
        );
        processed_fpl.sort_by_provider_ordinal();
        playlist_resolve(ctx, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_vod_resolve", target.name, provider_fpl.input.name).as_str(),
        );
        // stats
        let input_entry_name = processed_fpl.input.name.clone();
        let group_count = processed_fpl.get_group_count();
        let channel_count = processed_fpl.get_channel_count();
        if let Some(stat) = stats.get_mut(&input_entry_name) {
            stat.processed_stats.group_count = group_count;
            stat.processed_stats.channel_count = channel_count;
        }
        process_playlist_epg(&mut processed_fpl, &mut new_epg).await;
        log_memory_snapshot(
            format!("target '{}' input '{}' after_epg_apply", target.name, processed_fpl.input.name).as_str(),
        );
        new_playlist.extend(processed_fpl.source.take_groups());
        log_memory_snapshot(
            format!("target '{}' input '{}' after_take_groups", target.name, processed_fpl.input.name).as_str(),
        );
        tokio::task::yield_now().await;
    }
    step.tick("filter rename map + epg");
    log_memory_snapshot(format!("target '{}' after_filter_rename_map_epg", target.name).as_str());

    if target.favourites.is_some() {
        step.broadcast("Processing favourites for '{}' playlist", &target.name);
        process_favourites(&mut new_playlist, target.favourites.as_deref());
        log_memory_snapshot(format!("target '{}' after_favourites", target.name).as_str());
    }

    if new_playlist.is_empty() {
        step.stop("");
        info!("Playlist is empty: {}", &target.name);
        Ok(())
    } else {
        // Process Trakt categories
        if trakt_playlist(&ctx.client, target, errors, &mut new_playlist).await {
            step.tick("trakt categories");
            log_memory_snapshot(format!("target '{}' after_trakt", target.name).as_str());
        }

        let mut flat_new_playlist = flatten_groups(new_playlist);
        step.tick("playlist merge");
        log_memory_snapshot(format!("target '{}' after_playlist_merge", target.name).as_str());

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
        let result = persist_playlist(
            &ctx.config,
            &mut flat_new_playlist,
            flatten_tvguide(new_epg).as_ref(),
            target,
            ctx.playlist_state.as_ref(),
        )
        .await;
        step.stop("Persisting playlists");
        log_memory_snapshot(format!("target '{}' after_persist", target.name).as_str());
        result
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
        Some(StreamProperties::Live(l)) => l.video.is_some() && l.audio.is_some(),
        Some(StreamProperties::Episode(e)) => e.video.is_some() && e.audio.is_some(),
        Some(StreamProperties::Series(_)) | None => false,
    }
}

fn get_live_probe_interval_settings(
    target: &ConfigTarget,
    input_type: InputType,
    input_options: Option<&ConfigInputOptions>,
) -> Option<(u16, u64)> {
    if !(input_type.is_xtream() || input_type.is_m3u()) {
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
            if let Some(last_ts) = props.last_probed_timestamp {
                last_ts < cutoff_ts
            } else {
                true
            }
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

    for item in fpl.items() {
        if !is_probe_supported_item_type(item.header.item_type) {
            continue;
        }

        match item.header.item_type {
            PlaylistItemType::Live => {
                if !probe_live_enabled {
                    continue;
                }

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
                if !probe_vod_enabled {
                    continue;
                }
                // Xtream outputs handle VOD probe as part of the resolve pipeline (after resolve).
                if xtream_probe_handled {
                    continue;
                }
            }
            PlaylistItemType::Series | PlaylistItemType::LocalSeries => {
                if !probe_series_enabled {
                    continue;
                }
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
        error!("{}", err.message);
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
        events.send_event(EventMessage::PlaylistUpdate(shared::model::PlaylistUpdateState::Success));
    }

    let elapsed = start_time.elapsed().as_secs();
    let update_finished_message = format!("🌷 Update process finished! Took {elapsed} secs.");

    if let Some(events) = event_manager.as_deref() {
        events.send_event(EventMessage::PlaylistUpdateProgress(
            "Playlist Update".to_string(),
            update_finished_message.clone(),
        ));
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
    use crate::model::StagedInput;
    use shared::utils::Internable;

    fn item_with_props(props: StreamProperties) -> PlaylistItem {
        let header = shared::model::PlaylistItemHeader { additional_properties: Some(props), ..Default::default() };
        PlaylistItem { header }
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
    fn has_probe_details_requires_video_and_audio_for_live() {
        let live_missing_audio = shared::model::LiveStreamProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: None,
            ..Default::default()
        };
        let item_missing_audio = item_with_props(StreamProperties::Live(Box::new(live_missing_audio)));
        assert!(!has_probe_details(&item_missing_audio));

        let live_complete = shared::model::LiveStreamProperties {
            video: Some("{\"codec_name\":\"h264\"}".intern()),
            audio: Some("{\"codec_name\":\"aac\"}".intern()),
            ..Default::default()
        };
        let item_complete = item_with_props(StreamProperties::Live(Box::new(live_complete)));
        assert!(has_probe_details(&item_complete));
    }

    #[test]
    fn has_probe_details_is_false_for_series() {
        let series = shared::model::SeriesStreamProperties::default();
        let item = item_with_props(StreamProperties::Series(Box::new(series)));
        assert!(!has_probe_details(&item));
    }

    #[test]
    fn hybrid_detection_requires_xtream_main_and_m3u_staged() {
        let hybrid_input = ConfigInput {
            name: "hybrid".intern(),
            input_type: InputType::Xtream,
            staged: Some(StagedInput { enabled: true, input_type: InputType::M3u, ..StagedInput::default() }),
            ..ConfigInput::default()
        };
        assert!(is_hybrid_m3u_xtream(&hybrid_input));

        let non_hybrid_main_m3u = ConfigInput {
            name: "main_m3u".intern(),
            input_type: InputType::M3u,
            staged: Some(StagedInput { enabled: true, input_type: InputType::Xtream, ..StagedInput::default() }),
            ..ConfigInput::default()
        };
        assert!(!is_hybrid_m3u_xtream(&non_hybrid_main_m3u));
    }

    #[test]
    fn collect_effective_skip_clusters_includes_staged_skip_only_when_enabled() {
        let input_with_enabled_staged = ConfigInput {
            name: "enabled".intern(),
            input_type: InputType::Xtream,
            staged: Some(StagedInput {
                enabled: true,
                live_source: ClusterSource::Skip,
                vod_source: ClusterSource::Input,
                series_source: ClusterSource::Staged,
                ..StagedInput::default()
            }),
            ..ConfigInput::default()
        };
        let skip_enabled = collect_effective_skip_clusters(&input_with_enabled_staged);
        assert!(skip_enabled.contains(&XtreamCluster::Live));
        assert!(!skip_enabled.contains(&XtreamCluster::Video));
        assert!(!skip_enabled.contains(&XtreamCluster::Series));

        let input_with_disabled_staged = ConfigInput {
            name: "disabled".intern(),
            input_type: InputType::Xtream,
            staged: Some(StagedInput {
                enabled: false,
                live_source: ClusterSource::Skip,
                vod_source: ClusterSource::Skip,
                series_source: ClusterSource::Skip,
                ..StagedInput::default()
            }),
            ..ConfigInput::default()
        };
        assert!(collect_effective_skip_clusters(&input_with_disabled_staged).is_empty());
    }

    #[test]
    fn hybrid_m3u_detects_any_staged_cluster_selection() {
        let hybrid = ConfigInput {
            name: "hybrid".intern(),
            input_type: InputType::Xtream,
            staged: Some(StagedInput {
                enabled: true,
                input_type: InputType::M3u,
                live_source: ClusterSource::Input,
                vod_source: ClusterSource::Staged,
                series_source: ClusterSource::Skip,
                ..StagedInput::default()
            }),
            ..ConfigInput::default()
        };
        assert!(hybrid_has_m3u_staged_cluster(&hybrid, &[]));
        assert!(!hybrid_has_m3u_staged_cluster(&hybrid, &[XtreamCluster::Video]));
    }

    #[test]
    fn filter_skipped_clusters_removes_cached_groups() {
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

        let source = MemoryPlaylistSource::new(groups).boxed();
        let input = ConfigInput {
            name: "skip_live".intern(),
            input_type: InputType::Xtream,
            staged: Some(StagedInput {
                enabled: true,
                live_source: ClusterSource::Skip,
                ..StagedInput::default()
            }),
            ..ConfigInput::default()
        };

        let mut filtered = filter_skipped_clusters_from_source(source, &input);
        let filtered_groups = filtered.take_groups();
        assert_eq!(filtered_groups.len(), 1);
        assert_eq!(filtered_groups[0].xtream_cluster, XtreamCluster::Video);
    }
}
