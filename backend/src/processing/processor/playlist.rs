use crate::model::{AppConfig, ConfigFavourites, ConfigInput, ConfigRename, ReverseProxyDisabledHeaderConfig, TVGuide};
use crate::utils::m3u;
use crate::utils::xtream;
use crate::utils::{epg, StepMeasureCallback};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use tokio::sync::{Mutex, OwnedRwLockWriteGuard, RwLock};
use tokio::task::JoinSet;

use crate::api::model::{EventManager, EventMessage, PlaylistStorageState, UpdateGuard};
use crate::messaging::{send_message};
use crate::model::messaging::{MessageContent};
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
use crate::repository::{load_input_playlist, persist_input_playlist, persist_playlist};
use crate::repository::{CategoryKey, MemoryPlaylistSource, PlaylistSource};
use crate::utils::StepMeasure;
use crate::utils::{debug_if_enabled, trace_if_enabled};
use futures::StreamExt;
use indexmap::IndexMap;
use log::{debug, error, info, log_enabled, warn, Level};
use shared::error::{get_errors_notify_message, notify_err, TuliproxError};
use shared::foundation::filter::{get_field_value, set_field_value, Filter, ValueAccessor, ValueProvider};
use shared::model::xtream_const::XTREAM_CLUSTER;
use shared::model::{CounterModifier, FieldGetAccessor, FieldSetAccessor, InputType, ItemField,
                    PlaylistGroup, PlaylistItem, PlaylistItemType, PlaylistUpdateState,
                    ProcessingOrder, XtreamCluster};
use shared::utils::{create_alias_uuid, default_as_default, interner_gc, Internable};
use std::time::Instant;
use shared::concat_string;
use shared::model::UUIDType;

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
            let key = (cluster, group_title.clone());
            groups.entry(key)
                .or_insert_with(|| PlaylistGroup {
                    id: cat_id,
                    title: group_title,
                    channels: vec![],
                    xtream_cluster: cluster,
                })
                .channels.push(pli);
        }
    }

    if groups.is_empty() { None } else { Some(groups.into_values().collect()) }
}

fn filter_playlist(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    apply_filter_to_source(source, &target.filter)
}

pub fn apply_filter_to_playlist(playlist: &mut [PlaylistGroup], filter: &Filter) -> Option<Vec<PlaylistGroup>> {
    let mut new_playlist = Vec::with_capacity(128);
    for pg in playlist.iter_mut() {
        let channels = pg.channels.iter()
            .filter(|&pli| is_valid(pli, filter, false)).cloned().collect::<Vec<PlaylistItem>>();
        if !channels.is_empty() {
            new_playlist.push(PlaylistGroup {
                id: pg.id,
                title: pg.title.clone(),
                channels,
                xtream_cluster: pg.xtream_cluster,
            });
        }
    }
    if new_playlist.is_empty() { None } else { Some(new_playlist) }
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
                groups.entry((cluster, group_title.clone()))
                    .or_insert_with(|| PlaylistGroup {
                        id: cat_id,
                        title: group_title,
                        channels: vec![],
                        xtream_cluster: cluster,
                    })
                    .channels.push(pli);
            }
            Some(groups.into_values().collect())
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

fn map_playlist(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    let mapping_binding = target.mapping.load();
    let mappings = mapping_binding.as_ref()?;
    let valid_mappings = mappings.iter().filter(|m| m.mapper.as_ref().is_some_and(|v| !v.is_empty()));
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
        next_groups.entry((cluster, group_title.clone()))
            .or_insert_with(|| {
                grp_id += 1;
                PlaylistGroup {
                    id: grp_id,
                    title: group_title,
                    channels: Vec::new(),
                    xtream_cluster: cluster,
                }
            })
            .channels.push(channel);
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

async fn playlist_download_from_input(client: &reqwest::Client, app_config: &Arc<AppConfig>, input: &ConfigInput) -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool, bool) {
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
        return (vec![], vec![], true, false);
    }

    let (playlist, errors, persisted) = match input.input_type {
        InputType::M3u => {
            let (p, e) = m3u::download_m3u_playlist(app_config, client, config, input).await;
            (p, e, false)
        }
        InputType::Xtream => xtream::download_xtream_playlist(app_config, client, input, clusters_to_download.as_deref()).await,
        InputType::M3uBatch | InputType::XtreamBatch => (vec![], vec![], false),
        InputType::Library => {
            let (p, e) = library::download_library_playlist(client, app_config, input).await;
            (p, e, false)
        }
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

    (playlist, errors, false, persisted)
}

async fn process_source(source_idx: usize, ctx: &PlaylistProcessingContext) -> (Vec<InputStats>, Vec<TargetStats>, Vec<TuliproxError>) {
    let sources = ctx.config.sources.load();
    let mut errors = vec![];
    let mut input_stats = HashMap::<Arc<str>, InputStats>::new();
    let mut target_stats = Vec::<TargetStats>::new();
    if let Some(source) = sources.get_source_at(source_idx) {
        let mut source_playlists = Vec::with_capacity(128);
        let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());
        // Download the sources
        let mut source_downloaded = false;
        for input_name in &source.inputs {
            let Some(input) = sources.get_input_by_name(input_name) else {
                error!("Input {input_name} referenced by source {source_idx} does not exist");
                continue;
            };
            if is_input_enabled(input, &ctx.user_targets) {
                source_downloaded = true;

                let start_time = Instant::now();
                // Download the playlist for input
                let (mut playlist_groups, mut error_list) = {
                    broadcast_step("Playlist download", &format!("Downloading input '{}'", input.name));

                    let (mut download_err, playlist, error) = download_input(ctx, input).await;

                    if let Some(err) = error {
                        broadcast_step("Playlist download", &format!("Failed to persist/load input '{}' playlist", input.name));
                        error!("Failed to persist input playlist {}", input.name);
                        download_err.push(err);
                    }
                    (playlist, download_err)
                };

                let tvguide = if input.input_type == InputType::Library {
                    None
                } else {
                    download_input_epg(ctx, input, &mut error_list).await
                };

                errors.append(&mut error_list);
                let group_count = playlist_groups.get_group_count();
                let channel_count = playlist_groups.get_channel_count();
                let input_name = &input.name;
                if playlist_groups.is_empty() {
                    broadcast_step("Playlist download", &format!("Input '{}' playlist is empty", input.name));
                    info!("Source is empty {input_name}");
                    errors.push(notify_err!("Source is empty {input_name}"));
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
                input_stats.insert(input_name.clone(), create_input_stat(group_count, channel_count, errors.len(),
                                                                         input.input_type, input_name, elapsed));
            }
        }
        if source_downloaded {
            if source_playlists.is_empty() {
                debug!("Source at index {source_idx} is empty");
                errors.push(notify_err!("Source at index {source_idx} is empty: {}", source.inputs.iter().map(Clone::clone).collect::<Vec<Arc<str>>>().join(", ")));
            } else {
                debug_if_enabled!("Source has {} groups", source_playlists.iter_mut().map(FetchedPlaylist::get_channel_count).sum::<usize>());
                for target in &source.targets {
                    if is_target_enabled(target, &ctx.user_targets) {
                        match process_playlist_for_target(ctx, &mut source_playlists, target,
                                                          &mut input_stats, &mut errors).await {
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

async fn download_input_epg(ctx: &PlaylistProcessingContext, input: &Arc<ConfigInput>,
                            error_list: &mut Vec<TuliproxError>) -> Option<TVGuide> {
    // Download epg for input
    let (tvguide, mut tvguide_errors) = if error_list.is_empty() {
        let working_dir = &ctx.config.config.load().working_dir;
        epg::get_xmltv(ctx, input, None, working_dir).await
    } else {
        (None, vec![])
    };
    error_list.append(&mut tvguide_errors);
    tvguide
}

async fn download_input(ctx: &PlaylistProcessingContext, input: &Arc<ConfigInput>)
                        -> (Vec<TuliproxError>, Box<dyn PlaylistSource>, Option<TuliproxError>) {
    // Coordination Logic
    let need_download = !ctx.is_input_downloaded(&input.name).await;

    let (downloaded_playlist, download_err, was_cached, persisted) = if need_download {
        // Acquire named lock to prevent thundering herd on same input
        let _input_lock = ctx.get_input_lock(&input.name).await;
        // Check again after lock
        let already_processed = ctx.is_input_downloaded(&input.name).await;

        if already_processed {
            // Use empty results, will load from disk below
            (vec![], vec![], true, false)
        } else {
            let res = playlist_download_from_input(&ctx.client, &ctx.config, input).await;
            // Mark as processed if NO critical errors?
            // playlist_download_from_input returns errors but also potentially a partial playlist.
            // If it attempted download, we consider it processed for this session.
            ctx.mark_input_downloaded(input.name.clone()).await;
            res
        }
    } else {
        (vec![], vec![], true, false)
    };

    let (playlist, error) = if was_cached || persisted {
        match load_input_playlist(ctx, input, None).await {
            Ok(pl_source) => (pl_source, None),
            Err(e) => (MemoryPlaylistSource::default().boxed(), Some(e)),
        }
    } else {
        debug!("Persisting input '{}' playlist", input.name);
        let (pl, err) = persist_input_playlist(&ctx.config, input, downloaded_playlist).await;
        (MemoryPlaylistSource::new(pl).boxed(), err)
    };
    (download_err, playlist, error)
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


#[derive(Clone)]
pub struct PlaylistProcessingContext {
    pub client: reqwest::Client,
    pub config: Arc<AppConfig>,
    pub user_targets: Arc<ProcessTargets>,
    pub event_manager: Option<Arc<EventManager>>,
    pub playlist_state: Option<Arc<PlaylistStorageState>>,
    pub disabled_headers: Option<ReverseProxyDisabledHeaderConfig>,

    // Coordination
    processed_inputs: Arc<Mutex<HashSet<Arc<str>>>>,
    #[allow(clippy::type_complexity)]
    input_locks: Arc<Mutex<HashMap<Arc<str>, Weak<RwLock<()>>>>>,
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
        let lock = locks.get(input_name)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
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

        if process_parallel {
            async_tasks.spawn(async move {
                // Hold the per-source lock for the full duration of this update.
                let current_update_lock = update_lock;
                let (input_stats, target_stats, mut res_errors) =
                    process_source(index, &ctx).await;
                shared_errors.lock().await.append(&mut res_errors);
                if let Some(process_stats) = SourceStats::try_new(input_stats, target_stats) {
                    shared_stats.lock().await.push(process_stats);
                }
                drop(current_update_lock);
            });
        } else {
            let (input_stats, target_stats, mut res_errors) =
                process_source(index, &ctx).await;
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

pub type ProcessingPipe = Vec<fn(source: &mut dyn PlaylistSource, target: &ConfigTarget) -> Option<Vec<PlaylistGroup>>>;

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

fn execute_pipe<'a>(target: &ConfigTarget, pipe: &ProcessingPipe, fpl: &FetchedPlaylist<'a>,
                    duplicates: &mut HashSet<UUIDType>) -> FetchedPlaylist<'a> {
    let mut new_fpl = FetchedPlaylist {
        input: fpl.input,
        source: fpl.clone_source(),
        epg: fpl.epg.clone(),
    };
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
        let key = (group.xtream_cluster, group.title.clone());
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
async fn process_playlist_for_target(ctx: &PlaylistProcessingContext,
                                     playlists: &mut [FetchedPlaylist<'_>],
                                     target: &ConfigTarget,
                                     stats: &mut HashMap<Arc<str>, InputStats>,
                                     errors: &mut Vec<TuliproxError>,
) -> Result<(), Vec<TuliproxError>> {
    debug_if_enabled!("Processing order is {}", &target.processing_order);

    let mut duplicates: HashSet<UUIDType> = HashSet::new();
    let mut processed_fetched_playlists: Vec<FetchedPlaylist> = vec![];

    debug!("Executing processing pipes");
    let broadcast_step = create_broadcast_callback(ctx.event_manager.as_ref());

    let pipe = get_processing_pipe(target);
    let mut step = StepMeasure::new(&target.name, broadcast_step);
    for provider_fpl in playlists.iter_mut() {
        step.broadcast("Executing transformations on '{}' playlist", &target.name);
        let mut processed_fpl = execute_pipe(target, &pipe, provider_fpl, &mut duplicates);
        processed_fpl.sort_by_provider_ordinal();
        playlist_resolve_series(&ctx.config, &ctx.client, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        playlist_resolve_vod(&ctx.config, &ctx.client, target, errors, provider_fpl, &mut processed_fpl).await;
        // stats
        let input_entry_name = processed_fpl.input.name.clone();
        let group_count = processed_fpl.get_group_count();
        let channel_count = processed_fpl.get_channel_count();
        if let Some(stat) = stats.get_mut(&input_entry_name) {
            stat.processed_stats.group_count = group_count;
            stat.processed_stats.channel_count = channel_count;
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
        if trakt_playlist(&ctx.client, target, errors, &mut new_playlist).await {
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

        if process_watch(&ctx.config, &ctx.client, target, &flat_new_playlist).await {
            step.tick("group watches");
        }
        let result = persist_playlist(&ctx.config, &mut flat_new_playlist, flatten_tvguide(&new_epg).as_ref(), target, ctx.playlist_state.as_ref()).await;
        step.stop("Persisting playlists");
        result
    }
}

pub fn process_favourites(playlist: &mut Vec<PlaylistGroup>, favourites_cfg: Option<&[ConfigFavourites]>) {
    if let Some(favourites) = favourites_cfg {
        let mut fav_groups: IndexMap<CategoryKey, Vec<PlaylistItem>> = IndexMap::new();
        for pg in playlist.iter() {
            for pli in &pg.channels {
                // series episodes can't be included in favourites
                if pli.header.item_type == PlaylistItemType::Series || pli.header.item_type == PlaylistItemType::LocalSeries {
                    continue;
                }
                for fav in favourites {
                    if pli.header.xtream_cluster == fav.cluster && is_valid(pli, &fav.filter, fav.match_as_ascii) {
                        let mut channel = pli.clone();
                        channel.header.group.clone_from(&fav.group);
                        // Update UUID to be an alias of the original
                        channel.header.uuid = create_alias_uuid(&pli.header.uuid, &fav.group);
                        fav_groups
                            .entry((fav.cluster, fav.group.clone()))
                            .or_default()
                            .push(channel);
                    }
                }
            }
        }

        for (fav_group, channels) in fav_groups {
            if !channels.is_empty() {
                let (xtream_cluster, group_name) = fav_group;
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
    let mut new_playlist: Vec<PlaylistGroup> = vec![];
    let mut new_epg = vec![];

    // each fetched playlist can have its own epgl url.
    // we need to process each input epg.
    for fp in processed_fetched_playlists {
        process_playlist_epg(fp, &mut new_epg).await;
        new_playlist.extend(fp.source.take_groups());
    }
    (new_epg, new_playlist)
}

async fn process_watch(app_config: &Arc<AppConfig>, client: &reqwest::Client, target: &ConfigTarget, new_playlist: &[PlaylistGroup]) -> bool {
    if let Some(watches) = &target.watch {
        if default_as_default().eq_ignore_ascii_case(&target.name) {
            error!("can't watch a target with no unique name");
            return false;
        }

        futures::stream::iter(
            new_playlist
                .iter()
                .filter(|pl| watches.iter().any(|r| r.is_match(&pl.title)))
                .map(|pl| process_group_watch(app_config, client, &target.name, pl))
        ).for_each_concurrent(16, |f| f).await;

        true
    } else {
        false
    }
}

pub async fn exec_processing(client: &reqwest::Client, app_config: Arc<AppConfig>, targets: Arc<ProcessTargets>,
                             event_manager: Option<Arc<EventManager>>, playlist_state: Option<Arc<PlaylistStorageState>>,
                             update_guard: Option<UpdateGuard>,
                             disabled_headers: Option<ReverseProxyDisabledHeaderConfig>) {
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
    };

    let start_time = Instant::now();
    let (stats, errors) = process_sources(&ctx).await;
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
        if let Some(events) = &event_manager {
            events.send_event(EventMessage::PlaylistUpdate(PlaylistUpdateState::Failure));
        }
        send_message(&app_config, client, MessageContent::event_error(message)).await;
    } else if let Some(events) = &event_manager {
        events.send_event(EventMessage::PlaylistUpdate(PlaylistUpdateState::Success));
    }

    let elapsed = start_time.elapsed().as_secs();
    let update_finished_message = format!("🌷 Update process finished! Took {elapsed} secs.");

    if let Some(events) = &event_manager {
        events.send_event(EventMessage::PlaylistUpdateProgress("Playlist Update".to_string(), update_finished_message.clone()));
    }
    debug!("StringInterner GC removed {} strings", interner_gc());

    info!("{update_finished_message}");
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
