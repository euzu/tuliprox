use crate::model::{AppConfig, Config, ConfigFavourites, ConfigInput, ConfigRename};
use crate::utils::m3u;
use crate::utils::xtream;
use crate::utils::{epg, StepMeasureCallback};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::api::model::{EventManager, EventMessage, PlaylistStorageState, UpdateGuard};
use crate::messaging::send_message_json;
use crate::model::Epg;


use crate::model::FetchedPlaylist;
use crate::model::Mapping;
use crate::model::{ConfigTarget, ProcessTargets};
use crate::model::{InputStats, PlaylistStats, SourceStats, TargetStats};
use crate::processing::input_cache;
use crate::processing::input_cache::ClusterState;
use crate::processing::parser::xmltv::flatten_tvguide;
use crate::processing::playlist_watch::process_group_watch;
use crate::processing::processor::epg::process_playlist_epg;
use crate::processing::processor::library;
use crate::processing::processor::sort::sort_playlist;
use crate::processing::processor::trakt::process_trakt_categories_for_target;
use crate::processing::processor::xtream_series::playlist_resolve_series;
use crate::processing::processor::xtream_vod::playlist_resolve_vod;
use crate::repository::playlist_repository::{load_input_playlist, persist_input_playlist, persist_playlist};
use crate::repository::provider_source::ProviderPlaylistSource;
use crate::utils::StepMeasure;
use crate::utils::{debug_if_enabled, trace_if_enabled};
use futures::StreamExt;
use indexmap::IndexMap;
use log::{debug, error, info, log_enabled, trace, warn, Level};
use shared::error::{get_errors_notify_message, notify_err, TuliproxError};
use shared::foundation::filter::{get_field_value, set_field_value, Filter, ValueAccessor, ValueProvider};
use shared::model::xtream_const::XTREAM_CLUSTER;
use shared::model::{CounterModifier, FieldGetAccessor, FieldSetAccessor, InputType, ItemField, MsgKind, PlaylistEntry, PlaylistGroup, PlaylistItem, PlaylistItemType, PlaylistUpdateState, ProcessingOrder, UUIDType, XtreamCluster};
use shared::utils::{create_alias_uuid, default_as_default, StringInterner};
use std::time::Instant;

fn is_valid(pli: &PlaylistItem, filter: &Filter, match_as_ascii: bool) -> bool {
    let provider = ValueProvider { pli, match_as_ascii };
    filter.filter(&provider)
}

#[allow(clippy::unnecessary_wraps)]
pub fn apply_filter_to_playlist(playlist: &mut [PlaylistGroup], filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    debug!("Filtering {} groups", playlist.len());
    let mut new_playlist = Vec::with_capacity(128);
    for pg in playlist.iter_mut() {
        let channels = pg.channels.iter()
            .filter(|&pli| is_valid(pli, filter, false)).cloned().collect::<Vec<PlaylistItem>>();
        trace!("Filtered group {} has now {}/{} items", pg.title, channels.len(), pg.channels.len());
        if !channels.is_empty() {
            new_playlist.push(PlaylistGroup {
                id: pg.id,
                title: pg.title.clone(),
                channels,
                xtream_cluster: pg.xtream_cluster,
            });
        }
    }
    Some(new_playlist)
}

fn filter_playlist(playlist: &mut [PlaylistGroup], target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    apply_filter_to_playlist(playlist, &target.filter)
}

fn assign_channel_no_playlist(new_playlist: &mut [PlaylistGroup]) {
    let assigned_chnos: HashSet<u32> = new_playlist.iter().flat_map(|g| &g.channels)
        .filter(|c| c.header.chno != 0)
        .map(|c| c.header.chno)
        .collect();
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
                let cap = r.pattern.replace_all(value.as_str(), &r.new_name);
                if log_enabled!(log::Level::Debug) && *value != cap {
                    trace_if_enabled!("Renamed {}={value} to {cap}", &r.field);
                }
                let value = cap.into_owned();
                set_field_value(result, r.field, value);
            }
        }
    }
}

fn rename_playlist(playlist: &mut [PlaylistGroup], target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    match &target.rename {
        Some(renames) => {
            if !renames.is_empty() {
                let mut new_playlist: Vec<PlaylistGroup> = Vec::with_capacity(playlist.len());
                for g in playlist {
                    let mut grp = g.clone();
                    for r in renames {
                        if matches!(r.field, ItemField::Group) {
                            let cap = r.pattern.replace_all(&grp.title, &r.new_name);
                            trace_if_enabled!("Renamed group {} to {cap} for {}", &grp.title, target.name);
                            grp.title = cap.into_owned();
                        }
                    }

                    grp.channels.iter_mut().for_each(|pli| exec_rename(pli, target.rename.as_ref()));
                    new_playlist.push(grp);
                }
                return Some(new_playlist);
            }
            None
        }
        _ => None
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
                            let mut accessor = ValueAccessor { pli: ref_chan, virtual_items: vec![], match_as_ascii: mapping.match_as_ascii };
                            script.eval(&mut accessor, templates);
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

fn map_playlist(playlist: &mut [PlaylistGroup], target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    if let Some(mappings) = target.mapping.load().as_ref() {
        let new_playlist: Vec<PlaylistGroup> = playlist.iter().map(|playlist_group| {
            let mut grp = playlist_group.clone();
            mappings.iter().filter(|&mapping| mapping.mapper.as_ref().is_some_and(|v| !v.is_empty()))
                .for_each(|mapping|
                    grp.channels = grp.channels.drain(..).flat_map(|chan| map_channel_and_flatten(chan, mapping)).collect());
            grp
        }).collect();

        // if the group names are changed, restructure channels to the right groups
        // we use
        let mut new_groups: Vec<PlaylistGroup> = Vec::with_capacity(128);
        let mut grp_id: u32 = 0;
        for playlist_group in new_playlist {
            for channel in &playlist_group.channels {
                let cluster = &channel.header.xtream_cluster;
                let title = &channel.header.group;
                if let Some(grp) = new_groups.iter_mut().find(|x| *x.title == **title) {
                    grp.channels.push(channel.clone());
                } else {
                    grp_id += 1;
                    new_groups.push(PlaylistGroup {
                        id: grp_id,
                        title: title.to_string(),
                        channels: vec![channel.clone()],
                        xtream_cluster: *cluster,
                    });
                }
            }
        }
        Some(new_groups)
    } else {
        None
    }
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
                                    let value = channel.header.get_field(&counter.field).map_or_else(String::new, |field_value| field_value.to_string());
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

async fn playlist_download_from_input(client: &reqwest::Client, app_config: &Arc<AppConfig>, input: &ConfigInput) -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool) {
    let config = &*app_config.config.load();
    let working_dir = &config.working_dir;

    // Check Status
    let storage_path = input_cache::resolve_input_storage_path(working_dir, &input.name);
    let mut status = input_cache::load_input_status(&storage_path);
    let cache_duration = input.cache_duration_seconds;

    // Ensure data directory exists
    if !storage_path.exists() {
        let _ = std::fs::create_dir_all(&storage_path);
    }

    let (clusters_to_download, fully_cached) = match input.input_type {
        InputType::Xtream => {
            let mut to_download = vec![];
            for c in XTREAM_CLUSTER {
                if !input_cache::is_cache_valid(&status, &c.to_string(), cache_duration) {
                    to_download.push(c);
                }
            }
            if to_download.is_empty() {
                (None, true) // Everything cached
            } else {
                (Some(to_download), false)
            }
        }
        _ => {
            // M3U / Library
            if input_cache::is_cache_valid(&status, "default", cache_duration) {
                (None, true)
            } else {
                (None, false) // Download all
            }
        }
    };

    if fully_cached {
        return (vec![], vec![], true);
    }

    let (playlist, errors) = match input.input_type {
        InputType::M3u => m3u::download_m3u_playlist(client, config, input).await,
        InputType::Xtream => xtream::download_xtream_playlist(config, client, input, clusters_to_download.as_deref()).await,
        InputType::M3uBatch | InputType::XtreamBatch => (vec![], vec![]),
        InputType::Library => library::download_library_playlist(client, app_config, input).await,
    };

    // Update Status
    if errors.is_empty() {
        if let InputType::Xtream = input.input_type {
            if let Some(clusters) = clusters_to_download {
                for c in clusters {
                    input_cache::update_cluster_status(&mut status, &c.to_string(), ClusterState::Ok);
                }
            } else {
                // All clusters logic if None passed (implies all were invalid or first run)
                for c in XTREAM_CLUSTER {
                    input_cache::update_cluster_status(&mut status, &c.to_string(), ClusterState::Ok);
                }
            }
        } else {
            input_cache::update_cluster_status(&mut status, "default", ClusterState::Ok);
        }
        input_cache::save_input_status(&storage_path, &status);
    } else {
        // Mark failed?
        // We could mark specific clusters as failed if we knew which one failed.
        // For simplicity, if error, we don't update the timestamp (so it stays expired/invalid).
        // Or we mark as Failed.
        if let InputType::Xtream = input.input_type {
            if let Some(clusters) = clusters_to_download {
                for c in clusters {
                    // Optimistic: Only mark failed if we are sure?
                    // Currently just don't update the status to OK.
                    input_cache::update_cluster_status(&mut status, &c.to_string(), ClusterState::Failed);
                }
            }
            input_cache::save_input_status(&storage_path, &status);
        }
    }

    (playlist, errors, false)
}

async fn process_source(client: &reqwest::Client, app_config: Arc<AppConfig>, source_idx: usize,
                        user_targets: Arc<ProcessTargets>, event_manager: Option<Arc<EventManager>>,
                        playlist_state: Option<&Arc<PlaylistStorageState>>,
) -> (Vec<InputStats>, Vec<TargetStats>, Vec<TuliproxError>) {
    let sources = app_config.sources.load();
    let mut errors = vec![];
    let mut input_stats = HashMap::<String, InputStats>::new();
    let mut target_stats = Vec::<TargetStats>::new();
    if let Some(source) = sources.get_source_at(source_idx) {
        let mut source_playlists = Vec::with_capacity(128);
        let broadcast_step = create_broadcast_callback(event_manager.as_ref());
        // Download the sources
        let mut source_downloaded = false;
        for input_name in &source.inputs {
            let Some(input) = sources.get_input_by_name(input_name) else {
                error!("Input {input_name} referenced by source {source_idx} does not exist");
                continue;
            };
            if is_input_enabled(input, &user_targets) {
                source_downloaded = true;

                let start_time = Instant::now();
                // Download the playlist for input
                let (playlist_groups, mut error_list) = {
                    broadcast_step("Playlist download", &format!("Downloading input '{}'", input.name));
                    // Caching Logic
                    let (downloaded_playlist, mut download_err, was_cached) = playlist_download_from_input(client, &app_config, input).await;

                    let (playlist, error) = if was_cached {
                        match load_input_playlist(&app_config, input, None).await {
                            Ok(pl_source) => (pl_source, None),
                            Err(e) => (ProviderPlaylistSource::Memory(Vec::default()), Some(e)),
                        }
                    } else {
                        broadcast_step("Playlist download", &format!("Persisting input '{}' playlist", input.name));
                        let (pl, err) = persist_input_playlist(&app_config, input, downloaded_playlist).await;
                        (ProviderPlaylistSource::Memory(pl), err)
                    };

                    if let Some(err) = error {
                        broadcast_step("Playlist download", &format!("Failed to persist/load input '{}' playlist", input.name));
                        error!("Failed to persist input playlist {}", input.name);
                        download_err.push(err);
                    }
                    (playlist, download_err)
                };

                // Download epg for input
                let (tvguide, mut tvguide_errors) = if error_list.is_empty() {
                    broadcast_step("Playlist download", &format!("Downloading epg for input '{}'", input.name));
                    let working_dir = &app_config.config.load().working_dir;
                    epg::get_xmltv(client, input, working_dir).await
                } else {
                    (None, vec![])
                };

                errors.append(&mut error_list);
                errors.append(&mut tvguide_errors);
                let group_count = match &playlist_groups {
                    ProviderPlaylistSource::Memory(g) => g.len(),
                    _ => 0, // Is it worth - Stats for disk?
                };
                let channel_count = match &playlist_groups {
                    ProviderPlaylistSource::Memory(g) => g.iter().map(|group| group.channels.len()).sum(),
                    _ => 0,
                };
                let input_name = &input.name;
                if matches!(playlist_groups, ProviderPlaylistSource::Memory(ref g) if g.is_empty()) {
                    broadcast_step("Playlist download", &format!("Input '{}' playlist is empty", input.name));
                    info!("Source is empty {input_name}");
                    errors.push(notify_err!(format!("Source is empty {input_name}")));
                } else {
                    source_playlists.push(
                        FetchedPlaylist {
                            input,
                            source: playlist_groups,
                            epg: tvguide,
                        }
                    );
                }
                let elapsed = start_time.elapsed().as_secs();
                input_stats.insert(input_name.clone(), create_input_stat(group_count, channel_count, error_list.len(),
                                                                         input.input_type, input_name, elapsed));
            }
        }
        if source_downloaded {
            if source_playlists.is_empty() {
                debug!("Source at index {source_idx} is empty");
                errors.push(notify_err!(format!("Source at index {source_idx} is empty: {}", source.inputs.iter().map(std::string::String::as_str).collect::<Vec<&str>>().join(", "))));
            } else {
                debug_if_enabled!("Source has {} groups", source_playlists.iter().map(|fpl| get_memory_source_count(fpl)).sum::<usize>());
                let event_manager_clone = event_manager.clone();
                for target in &source.targets {
                    let event_manager_clone = event_manager_clone.clone();
                    if is_target_enabled(target, &user_targets) {
                        match process_playlist_for_target(&app_config, client, &mut source_playlists, target, &mut input_stats, &mut errors, event_manager_clone, playlist_state).await {
                            Ok(()) => {
                                target_stats.push(TargetStats::success(&target.name));
                            }
                            Err(mut err) => {
                                target_stats.push(TargetStats::failure(&target.name));
                                errors.append(&mut err);
                            }
                        }
                    }
                }
            }
        }
    }
    (input_stats.into_values().collect(), target_stats, errors)
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

fn create_input_stat(group_count: usize, channel_count: usize, error_count: usize, input_type: InputType, input_name: &str, secs_took: u64) -> InputStats {
    InputStats {
        name: input_name.to_string(),
        input_type,
        error_count,
        raw_stats: PlaylistStats {
            group_count,
            channel_count,
        },
        processed_stats: PlaylistStats {
            group_count: 0,
            channel_count: 0,
        },
        secs_took,
    }
}

async fn process_sources(client: &reqwest::Client, config: &Arc<AppConfig>, user_targets: Arc<ProcessTargets>,
                         event_manager: Option<Arc<EventManager>>, playlist_state: Option<&Arc<PlaylistStorageState>>,
) -> (Vec<SourceStats>, Vec<TuliproxError>) {
    let mut async_tasks = JoinSet::new();
    let sources = config.sources.load();
    let process_parallel = config.config.load().process_parallel && sources.sources.len() > 1;
    if process_parallel && log_enabled!(Level::Debug) {
        debug!("Parallel processing enabled");
    }
    let errors = Arc::new(Mutex::<Vec<TuliproxError>>::new(vec![]));
    let stats = Arc::new(Mutex::<Vec<SourceStats>>::new(vec![]));
    for (index, source) in sources.sources.iter().enumerate() {
        if !source.should_process_for_user_targets(&user_targets) {
            continue;
        }

        // We're using the file lock this way on purpose
        let source_lock_path = PathBuf::from(format!("source_{index}"));
        let Ok(update_lock) = config.file_locks.try_write_lock(&source_lock_path).await else {
            warn!("The update operation for the source at index {index} was skipped because an update is already in progress.");
            continue;
        };

        let shared_errors = errors.clone();
        let shared_stats = stats.clone();
        let cfg = config.clone();
        let usr_trgts = user_targets.clone();
        let event_manager = event_manager.clone();
        if process_parallel {
            let http_client = client.clone();
            let playlist_state = playlist_state.cloned();
            async_tasks.spawn(async move {
                // Hold the per-source lock for the full duration of this update.
                let current_update_lock = update_lock;
                let (input_stats, target_stats, mut res_errors) =
                    process_source(&http_client, cfg, index, usr_trgts, event_manager, playlist_state.as_ref()).await;
                shared_errors.lock().await.append(&mut res_errors);
                if let Some(process_stats) = SourceStats::try_new(input_stats, target_stats) {
                    shared_stats.lock().await.push(process_stats);
                }
                drop(current_update_lock);
            });
        } else {
            let (input_stats, target_stats, mut res_errors) =
                process_source(client, cfg, index, usr_trgts, event_manager, playlist_state).await;
            shared_errors.lock().await.append(&mut res_errors);
            if let Some(process_stats) = SourceStats::try_new(input_stats, target_stats) {
                shared_stats.lock().await.push(process_stats);
            }
            drop(update_lock);
        }
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

pub type ProcessingPipe = Vec<fn(playlist: &mut [PlaylistGroup], target: &ConfigTarget) -> Option<Vec<PlaylistGroup>>>;

fn get_processing_pipe(target: &ConfigTarget) -> ProcessingPipe {
    match &target.processing_order {
        ProcessingOrder::Frm => vec![filter_playlist, rename_playlist, map_playlist],
        ProcessingOrder::Fmr => vec![filter_playlist, map_playlist, rename_playlist],
        ProcessingOrder::Rfm => vec![rename_playlist, filter_playlist, map_playlist],
        ProcessingOrder::Rmf => vec![rename_playlist, map_playlist, filter_playlist],
        ProcessingOrder::Mfr => vec![map_playlist, filter_playlist, rename_playlist],
        ProcessingOrder::Mrf => vec![map_playlist, rename_playlist, filter_playlist]
    }
}


fn execute_pipe_memory<'a>(target: &ConfigTarget, _pipe: &ProcessingPipe, fpl: &FetchedPlaylist<'a>,
                           duplicates: &mut HashSet<UUIDType>) -> FetchedPlaylist<'a> {
    let mut accumulator: IndexMap<Arc<str>, PlaylistGroup> = IndexMap::new();
    let mut group_id_counter = 0;
    let mut interner = StringInterner::new();

    if let ProviderPlaylistSource::Memory(groups) = &fpl.source {
        for group in groups {
            for channel in &group.channels {
                process_and_accumulate_item(channel.clone(), target, &mut accumulator, &mut group_id_counter, duplicates, &mut interner);
            }
        }
    }

    let source_groups = accumulator.into_values().collect();

    FetchedPlaylist {
        input: fpl.input,
        source: ProviderPlaylistSource::Memory(source_groups),
        epg: fpl.epg.clone(),
    }
}

fn process_and_accumulate_item(
    mut pli: PlaylistItem,
    target: &ConfigTarget,
    accumulator: &mut IndexMap<Arc<str>, PlaylistGroup>,
    group_id_counter: &mut u32,
    duplicates: &mut HashSet<UUIDType>,
    _interner: &mut StringInterner, // interner is not used here, but kept for consistency if needed later
) {
    // Filter
    if !is_valid(&pli, &target.filter, false) { return; }

    // Rename
    exec_rename(&mut pli, target.rename.as_ref());
    if let Some(renames) = &target.rename {
        for r in renames {
            if matches!(r.field, ItemField::Group) {
                let val = &pli.header.group;
                let cap = r.pattern.replace_all(val, &r.new_name);
                if **val != *cap {
                    pli.header.group = shared::utils::intern(&cap);
                }
            }
        }
    }

    // Mapping
    let items = if let Some(mappings) = target.mapping.load().as_ref() {
        let mut current_items = vec![pli];
        for mapping in mappings.iter().filter(|&m| m.mapper.as_ref().is_some_and(|v| !v.is_empty())) {
            let mut next_items = Vec::with_capacity(current_items.len());
            for item in current_items {
                next_items.extend(map_channel_and_flatten(item, mapping));
            }
            current_items = next_items;
        }
        current_items
    } else {
        vec![pli]
    };

    // De-duplication and Accumulation
    for item in items {
        if target.options.as_ref().is_some_and(|opt| opt.remove_duplicates)
            && !duplicates.insert(item.get_uuid()) { continue; }

        let group_name: Arc<str> = Arc::clone(&item.header.group);
        let cluster = item.header.xtream_cluster;

        let entry = accumulator.entry(group_name.clone()).or_insert_with(|| {
            *group_id_counter += 1;
            PlaylistGroup {
                id: *group_id_counter,
                title: group_name.to_string(),
                channels: Vec::new(),
                xtream_cluster: cluster,
            }
        });
        entry.channels.push(item);
    }
}

// Streaming implementation
fn execute_pipe_stream(target: &ConfigTarget, fpl: &mut FetchedPlaylist<'_>, duplicates: &mut HashSet<UUIDType>) -> Vec<PlaylistGroup> {
    let mut accumulator: IndexMap<Arc<str>, PlaylistGroup> = IndexMap::new();
    let mut group_id_counter = 0;
    let mut interner = StringInterner::new();

    match &mut fpl.source {
        ProviderPlaylistSource::XtreamDisk { live, vod, series, .. } => {
            if let Some(q) = live {
                for (_, item) in q.iter() {
                    process_and_accumulate_item(PlaylistItem::from(&item), target, &mut accumulator, &mut group_id_counter, duplicates, &mut interner);
                }
            }
            if let Some(q) = vod {
                for (_, item) in q.iter() {
                    process_and_accumulate_item(PlaylistItem::from(&item), target, &mut accumulator, &mut group_id_counter, duplicates, &mut interner);
                }
            }
            if let Some(q) = series {
                for (_, item) in q.iter() {
                    process_and_accumulate_item(PlaylistItem::from(&item), target, &mut accumulator, &mut group_id_counter, duplicates, &mut interner);
                }
            }
        }
        ProviderPlaylistSource::M3uDisk { query, .. } => {
            for (_, item) in query.iter() {
                process_and_accumulate_item(PlaylistItem::from(&item), target, &mut accumulator, &mut group_id_counter, duplicates, &mut interner);
            }
        }
        ProviderPlaylistSource::Memory(_) => {}
    }

    accumulator.into_values().collect()
}

// Helper to convert a source if needed or just use passed
fn get_memory_source_count(fpl: &FetchedPlaylist) -> usize {
    if let ProviderPlaylistSource::Memory(groups) = &fpl.source {
        groups.len()
    } else {
        0
    }
}

// This method is needed, because of duplicate group names in different inputs.
// We merge the same group names considering cluster together.
fn flatten_groups(playlistgroups: Vec<PlaylistGroup>) -> Vec<PlaylistGroup> {
    let mut sort_order: Vec<PlaylistGroup> = vec![];
    let mut idx: usize = 0;
    let mut group_map: HashMap<(String, XtreamCluster), usize> = HashMap::new();
    for group in playlistgroups {
        let key = (group.title.clone(), group.xtream_cluster);
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
async fn process_playlist_for_target(app_config: &Arc<AppConfig>,
                                     client: &reqwest::Client,
                                     playlists: &mut [FetchedPlaylist<'_>],
                                     target: &ConfigTarget,
                                     stats: &mut HashMap<String, InputStats>,
                                     errors: &mut Vec<TuliproxError>,
                                     event_manager: Option<Arc<EventManager>>,
                                     playlist_state: Option<&Arc<PlaylistStorageState>>,
) -> Result<(), Vec<TuliproxError>> {
    debug_if_enabled!("Processing order is {}", &target.processing_order);

    let mut duplicates: HashSet<UUIDType> = HashSet::new();
    let mut processed_fetched_playlists: Vec<FetchedPlaylist> = vec![];

    debug!("Executing processing pipes");
    let broadcast_step = create_broadcast_callback(event_manager.as_ref());

    let pipe = get_processing_pipe(target);
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    for provider_fpl in playlists.iter_mut() {
        step.broadcast("Executing transformations on '{}' playlist", &target.name);

        // New Streaming / Hybrid Pipeline
        let mut processed_fpl: FetchedPlaylist;

        // Avoid holding borrow of source when calling execute_pipe_stream
        let is_memory = matches!(provider_fpl.source, ProviderPlaylistSource::Memory(_));

        if is_memory {
            processed_fpl = execute_pipe_memory(target, &pipe, provider_fpl, &mut duplicates);
        } else {
            // Disk Source Streaming
            let groups = execute_pipe_stream(target, provider_fpl, &mut duplicates);
            processed_fpl = FetchedPlaylist {
                input: provider_fpl.input,
                source: ProviderPlaylistSource::Memory(groups),
                epg: provider_fpl.epg.clone(),
            };
        }


        playlist_resolve_series(app_config, client, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        playlist_resolve_vod(app_config, client, target, errors, &mut processed_fpl).await;
        // stats
        let input_stats = stats.get_mut(&processed_fpl.input.name);
        if let Some(stat) = input_stats {
            if let ProviderPlaylistSource::Memory(groups) = &processed_fpl.source {
                stat.processed_stats.group_count = groups.len();
                stat.processed_stats.channel_count = groups.iter()
                    .map(|group| group.channels.len())
                    .sum();
            }
        }
        processed_fetched_playlists.push(processed_fpl);
    }
    step.tick("filter rename map");
    let (new_epg, mut new_playlist) = process_epg(&mut processed_fetched_playlists).await;
    step.tick("epg");

    if target.favourites.is_some() {
        step.broadcast("Processing favourites for '{}' playlist", &target.name);
        process_favourites(&mut new_playlist, target.favourites.as_deref());
    }

    if new_playlist.is_empty() {
        step.stop("");
        info!("Playlist is empty: {}", &target.name);
        Ok(())
    } else {
        // Process Trakt categories
        if trakt_playlist(client, target, errors, &mut new_playlist).await {
            step.tick("trakt categories");
        }

        let mut flat_new_playlist = flatten_groups(new_playlist);
        step.tick("playlist merge");

        if sort_playlist(target, &mut flat_new_playlist) {
            step.tick("playlist sort");
        }
        assign_channel_no_playlist(&mut flat_new_playlist);
        step.tick("assigning channel numbers");
        map_playlist_counter(target, &mut flat_new_playlist);
        step.tick("assigning channel counter");

        let config = app_config.config.load();
        if process_watch(&config, client, target, &flat_new_playlist).await {
            step.tick("group watches");
        }
        let result = persist_playlist(app_config, &mut flat_new_playlist, flatten_tvguide(&new_epg).as_ref(), target, playlist_state).await;
        step.stop("Persisting playlists");
        result
    }
}

pub fn process_favourites(playlist: &mut Vec<PlaylistGroup>, favourites_cfg: Option<&[ConfigFavourites]>) {
    if let Some(favourites) = favourites_cfg {
        let mut fav_groups: IndexMap<String, Vec<PlaylistItem>> = IndexMap::new();
        for pg in playlist.iter() {
            for pli in &pg.channels {
                // series episodes cant be included in favourites
                if pli.header.item_type == PlaylistItemType::Series || pli.header.item_type == PlaylistItemType::LocalSeries {
                    continue;
                }
                for fav in favourites {
                    if is_valid(pli, &fav.filter, fav.match_as_ascii) {
                        let mut channel = pli.clone();
                        channel.header.group = shared::utils::intern(&fav.group);
                        // Update UUID to be an alias of the original
                        channel.header.uuid = create_alias_uuid(&pli.header.uuid, &fav.group);
                        fav_groups
                            .entry(fav.group.clone())
                            .or_default()
                            .push(channel);
                    }
                }
            }
        }

        for (group_name, channels) in fav_groups {
            if !channels.is_empty() {
                let xtream_cluster = channels[0].header.xtream_cluster;
                playlist.push(PlaylistGroup {
                    id: 0,
                    title: group_name,
                    channels,
                    xtream_cluster,
                });
            }
        }
    }
}

async fn trakt_playlist(client: &reqwest::Client, target: &ConfigTarget, errors: &mut Vec<TuliproxError>, playlist: &mut Vec<PlaylistGroup>) -> bool {
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

async fn process_epg(processed_fetched_playlists: &mut Vec<FetchedPlaylist<'_>>) -> (Vec<Epg>, Vec<PlaylistGroup>) {
    let mut new_playlist = vec![];
    let mut new_epg = vec![];

    // each fetched playlist can have its own epgl url.
    // we need to process each input epg.
    for fp in processed_fetched_playlists {
        process_playlist_epg(fp, &mut new_epg).await;
        if let ProviderPlaylistSource::Memory(groups) = &mut fp.source {
            new_playlist.append(groups);
        } else {
            warn!("Unexpected disk source in EPG pipeline for input: {}", fp.input.name);
        }
    }
    (new_epg, new_playlist)
}

async fn process_watch(cfg: &Config, client: &reqwest::Client, target: &ConfigTarget, new_playlist: &[PlaylistGroup]) -> bool {
    if let Some(watches) = &target.watch {
        if default_as_default().eq_ignore_ascii_case(&target.name) {
            error!("can't watch a target with no unique name");
            return false;
        }

        futures::stream::iter(
            new_playlist
                .iter()
                .filter(|pl| watches.iter().any(|r| r.is_match(&pl.title)))
                .map(|pl| process_group_watch(client, cfg, &target.name, pl))
        ).for_each_concurrent(16, |f| f).await;

        true
    } else {
        false
    }
}

pub async fn exec_processing(client: &reqwest::Client, app_config: Arc<AppConfig>, targets: Arc<ProcessTargets>,
                             event_manager: Option<Arc<EventManager>>, playlist_state: Option<Arc<PlaylistStorageState>>,
                             update_guard: Option<UpdateGuard>) {
    let _guard = if let Some(guard) = update_guard {
        if let Some(permit) = guard.try_playlist() {
            Some(permit)
        } else {
            warn!("Playlist update already in progress; update skipped.");
            if let Some(events) = event_manager.as_ref() {
                events.send_event(EventMessage::PlaylistUpdate(PlaylistUpdateState::Failure));
            }
            return;
        }
    } else {
        None
    };

    let event_manager_clone = event_manager.clone();
    let start_time = Instant::now();
    let (stats, errors) = process_sources(client, &app_config, targets.clone(), event_manager_clone, playlist_state.as_ref()).await;
    // log errors
    for err in &errors {
        error!("{}", err.message);
    }
    let config = app_config.config.load();
    let messaging = config.messaging.as_ref();

    if !stats.is_empty() {
        match serde_json::to_value(&stats) {
            Ok(val) => {
                match serde_json::to_string(&serde_json::Value::Object(
                    serde_json::map::Map::from_iter([("stats".to_string(), val)]))) {
                    Ok(stats_msg) => {
                        // print stats
                        info!("{stats_msg}");
                        // send stats
                        send_message_json(client, MsgKind::Stats, messaging, stats_msg.as_str()).await;
                    }
                    Err(err) => error!("Failed to serialize playlist stats {err}"),
                }
            }
            Err(err) => error!("Failed to serialize playlist stats {err}")
        }
    }

    // send errors
    if let Some(message) = get_errors_notify_message!(errors, 255) {
        if let Some(events) = event_manager {
            events.send_event(EventMessage::PlaylistUpdate(PlaylistUpdateState::Failure));
        }
        if let Ok(error_msg) = serde_json::to_string(&serde_json::Value::Object(serde_json::map::Map::from_iter([("errors".to_string(), serde_json::Value::String(message))]))) {
            send_message_json(client, MsgKind::Error, messaging, error_msg.as_str()).await;
        }
    } else if let Some(events) = event_manager {
        events.send_event(EventMessage::PlaylistUpdate(PlaylistUpdateState::Success));
    }
    let elapsed = start_time.elapsed().as_secs();
    info!("🌷 Update process finished! Took {elapsed} secs.");
}

// #[cfg(test)]
// mod tests {
// #[test]
// fn test_jaro_winkeler() {
//     let data = [("yessport5", "heyessport5gold"), ("yessport5", "heyesport5gold")];
//
//     data.iter().for_each(|(first, second)|
//     println!("jaro_winkler {} = {} => {}", first, second, strsim::jaro_winkler(first, second)));
//     // println!("jaro {}", strsim::jaro(data.0, data.1));
//     // println!("levenhstein {}", strsim::levenshtein(data.0, data.1));
//     // println!("damerau_levenshtein {:?}", strsim::damerau_levenshtein(data.0, data.1));
//     // println!("osa distance {:?}", strsim::osa_distance(data.0, data.1));
//     // println!("sorensen dice {:?}", strsim::sorensen_dice(data.0, data.1));
// }

// }
