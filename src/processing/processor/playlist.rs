use crate::model::{ConfigInput, ConfigRename};
use crate::utils::epg;
use crate::utils::m3u;
use crate::utils::xtream;
use crate::Config;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::sync::Mutex;

use crate::foundation::filter::{get_field_value, set_field_value, ValueProvider, ValueAccessor};
use crate::messaging::{send_message, MsgKind};
use crate::model::{ConfigTarget, InputType, ItemField, ProcessTargets, ProcessingOrder};
use crate::model::{CounterModifier, Mapping};
use crate::model::{FetchedPlaylist, FieldGetAccessor, FieldSetAccessor, PlaylistEntry, PlaylistGroup, PlaylistItem, UUIDType, XtreamCluster};
use crate::model::{InputStats, PlaylistStats, SourceStats, TargetStats};
use crate::processing::playlist_watch::process_group_watch;
use crate::processing::processor::xtream_series::playlist_resolve_series;
use crate::processing::processor::xtream_vod::playlist_resolve_vod;
use crate::repository::playlist_repository::persist_playlist;
use crate::tuliprox_error::{get_errors_notify_message, notify_err, TuliproxError, TuliproxErrorKind};
use crate::utils::debug_if_enabled;
use crate::utils::default_as_default;
use deunicode::deunicode;
use log::{debug, error, info, log_enabled, trace, warn, Level};
use std::time::Instant;
use crate::model::Epg;
use crate::processing::parser::xmltv::flatten_tvguide;
use crate::processing::processor::epg::process_playlist_epg;
use crate::processing::processor::sort::sort_playlist;
use crate::utils::StepMeasure;

fn is_valid(pli: &PlaylistItem, target: &ConfigTarget) -> bool {
    let provider = ValueProvider { pli };
    target.filter(&provider)
}

#[allow(clippy::unnecessary_wraps)]
fn filter_playlist(playlist: &mut [PlaylistGroup], target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    debug!("Filtering {} groups", playlist.len());
    let mut new_playlist = Vec::with_capacity(128);
    for pg in playlist.iter_mut() {
        let channels = pg.channels.iter()
            .filter(|&pli| is_valid(pli, target)).cloned().collect::<Vec<PlaylistItem>>();
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


fn assign_channel_no_playlist(new_playlist: &mut [PlaylistGroup]) {
    let assigned_chnos: HashSet<u32> = new_playlist.iter().flat_map(|g| &g.channels)
        .filter(|c| !c.header.chno.is_empty())
        .map(|c| c.header.chno.as_str())
        .flat_map(str::parse::<u32>).collect();
    let mut chno = 1;
    for group in new_playlist {
        for chan in &mut group.channels {
            if chan.header.chno.is_empty() {
                while assigned_chnos.contains(&chno) {
                    chno += 1;
                }
                chan.header.chno = chno.to_string();
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
                let value = get_field_value(result, &r.field);
                let cap = r.re.as_ref().unwrap().replace_all(value.as_str(), &r.new_name);
                if log_enabled!(log::Level::Debug) && *value != cap {
                    debug_if_enabled!("Renamed {}={} to {}", &r.field, value, cap);
                }
                let value = cap.into_owned();
                set_field_value(result, &r.field, value);
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
                            let cap = r.re.as_ref().unwrap().replace_all(&grp.title, &r.new_name);
                            debug_if_enabled!("Renamed group {} to {} for {}", &grp.title, cap, target.name);
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

fn map_channel(mut channel: PlaylistItem, mapping: &Mapping) -> PlaylistItem {
    if let Some(mapper) = &mapping.mapper {
        if !mapper.is_empty() {
            let header = &channel.header;
            let channel_name = if mapping.match_as_ascii { deunicode(&header.name) } else { header.name.to_string() };
            if mapping.match_as_ascii && log_enabled!(Level::Trace) { trace!("Decoded {} for matching to {}", &header.name, &channel_name); }
            let ref_chan = &mut channel;
            for m in mapper {
                if let Some(script) = m.t_script.as_ref() {
                    let provider = ValueProvider { pli: &ref_chan.clone() };
                    let mut accessor = ValueAccessor { pli: ref_chan };
                    if let Some(filter) = &m.t_filter {
                        if filter.filter(&provider) {
                            if let Err(err) = script.eval(&mut accessor) {
                                error!("Error while evaluating script: {err}");
                            }
                        }
                    }
                }
            }
        }
    }
    channel
}

fn map_playlist(playlist: &mut [PlaylistGroup], target: &ConfigTarget) -> Option<Vec<PlaylistGroup>> {
    if let Some(mappings) = target.t_mapping.load().as_ref() {
        let new_playlist: Vec<PlaylistGroup> = playlist.iter().map(|playlist_group| {
            let mut grp = playlist_group.clone();
            mappings.iter().filter(|&mapping| mapping.mapper.as_ref().is_some_and(|v| !v.is_empty()))
                .for_each(|mapping|
                    grp.channels = grp.channels.drain(..).map(|chan| map_channel(chan, mapping)).collect());
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
    if target.t_mapping.load().is_some() {
        let guard = target.t_mapping.load();
        let mappings = guard.as_ref().unwrap();
        for mapping in mappings.iter() {
            if let Some(counter_list) = &mapping.t_counter {
                for counter in counter_list {
                    for plg in &mut *playlist {
                        for channel in &mut plg.channels {
                            let provider = ValueProvider { pli: channel };
                            if counter.filter.filter(&provider) {
                                let cntval = counter.value.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
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

// If no input is enabled but the user set the target as command line argument,
// we force the input to be enabled.
// If there are enabled input, then only these are used.
fn is_input_enabled(input: &ConfigInput, user_targets: &ProcessTargets) -> bool {
    let input_enabled = input.enabled;
    let input_id = input.id;
    (!user_targets.enabled && input_enabled) || user_targets.has_input(input_id)
}

fn is_target_enabled(target: &ConfigTarget, user_targets: &ProcessTargets) -> bool {
    (!user_targets.enabled && target.enabled) || (user_targets.enabled && user_targets.has_target(target.id))
}

async fn process_source(client: Arc<reqwest::Client>, cfg: Arc<Config>, source_idx: usize, user_targets: Arc<ProcessTargets>) -> (Vec<InputStats>, Vec<TargetStats>, Vec<TuliproxError>) {
    let source = cfg.sources.get(source_idx).unwrap();
    let mut errors = vec![];
    let mut input_stats = HashMap::<String, InputStats>::new();
    let mut target_stats = Vec::<TargetStats>::new();
    let mut source_playlists = Vec::with_capacity(128);
    // Download the sources
    let mut source_downloaded = false;
    for input in &source.inputs {
        if is_input_enabled(input, &user_targets) {
            source_downloaded = true;
            let start_time = Instant::now();
            let (mut playlistgroups, mut error_list) = match input.input_type {
                InputType::M3u => m3u::get_m3u_playlist(Arc::clone(&client), &cfg, input, &cfg.working_dir).await,
                InputType::Xtream => xtream::get_xtream_playlist(&cfg, Arc::clone(&client), input, &cfg.working_dir).await,
                InputType::M3uBatch | InputType::XtreamBatch => (vec![], vec![])
            };
            let (tvguide, mut tvguide_errors) = if error_list.is_empty() {
                epg::get_xmltv(Arc::clone(&client), &cfg, input, &cfg.working_dir).await
            } else {
                (None, vec![])
            };
            errors.append(&mut error_list);
            errors.append(&mut tvguide_errors);
            let group_count = playlistgroups.len();
            let channel_count = playlistgroups.iter()
                .map(|group| group.channels.len())
                .sum();
            let input_name = &input.name;
            if playlistgroups.is_empty() {
                info!("Source is empty {input_name}");
                errors.push(notify_err!(format!("Source is empty {input_name}")));
            } else {
                playlistgroups.iter_mut().for_each(PlaylistGroup::on_load);
                source_playlists.push(
                    FetchedPlaylist {
                        input,
                        playlistgroups,
                        epg: tvguide,
                    }
                );
            }
            let elapsed = start_time.elapsed().as_secs();
            input_stats.insert(input_name.to_string(), create_input_stat(group_count, channel_count, error_list.len(),
                                                                         input.input_type, input_name, elapsed));
        }
    }
    if source_downloaded {
        if source_playlists.is_empty() {
            debug!("Source at index {source_idx} is empty");
            errors.push(notify_err!(format!("Source at {source_idx} is empty")));
        } else {
            debug_if_enabled!("Source has {} groups", source_playlists.iter().map(|fpl| fpl.playlistgroups.len()).sum::<usize>());
            for target in &source.targets {
                if is_target_enabled(target, &user_targets) {
                    match process_playlist_for_target(Arc::clone(&client), &mut source_playlists, target, &cfg, &mut input_stats, &mut errors).await {
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
    (input_stats.into_values().collect(), target_stats, errors)
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

async fn process_sources(client: Arc<reqwest::Client>, config: Arc<Config>, user_targets: Arc<ProcessTargets>) -> (Vec<SourceStats>, Vec<TuliproxError>) {
    let mut handle_list = vec![];
    let thread_num = config.threads;
    let process_parallel = thread_num > 1 && config.sources.len() > 1;
    if process_parallel && log_enabled!(Level::Debug) {
        debug!("Using {thread_num} threads");
    }
    let errors = Arc::new(Mutex::<Vec<TuliproxError>>::new(vec![]));
    let stats = Arc::new(Mutex::<Vec<SourceStats>>::new(vec![]));
    for (index, _) in config.sources.iter().enumerate() {
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
        if process_parallel {
            let http_client = Arc::clone(&client);
            let handles = &mut handle_list;
            let process = move || {
                // TODO better way ?
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    let (input_stats, target_stats, mut res_errors) = process_source(Arc::clone(&http_client), cfg, index, usr_trgts).await;
                    shared_errors.lock().await.append(&mut res_errors);
                    let process_stats = SourceStats::new(input_stats, target_stats);
                    shared_stats.lock().await.push(process_stats);
                });
            };
            handles.push(thread::spawn(process));
            if handles.len() >= thread_num as usize {
                handles.drain(..).for_each(|handle| { let _ = handle.join(); });
            }
        } else {
            let (input_stats, target_stats, mut res_errors) = process_source(Arc::clone(&client), cfg, index, usr_trgts).await;
            shared_errors.lock().await.append(&mut res_errors);
            let process_stats = SourceStats::new(input_stats, target_stats);
            shared_stats.lock().await.push(process_stats);
        }
        drop(update_lock);
    }
    for handle in handle_list {
        let _ = handle.join();
    }
    (Arc::try_unwrap(stats).unwrap().into_inner(), Arc::try_unwrap(errors).unwrap().into_inner())
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

fn duplicate_hash(item: &PlaylistItem) -> UUIDType {
    item.get_uuid()
}

fn execute_pipe<'a>(target: &ConfigTarget, pipe: &ProcessingPipe, fpl: &FetchedPlaylist<'a>, duplicates: &mut HashSet<UUIDType>) -> FetchedPlaylist<'a> {
    let mut new_fpl = FetchedPlaylist {
        input: fpl.input,
        playlistgroups: fpl.playlistgroups.clone(), // we need to clone, because of multiple target definitions, we cant change the initial playlist.
        epg: fpl.epg.clone(),
    };
    if target.options.as_ref().is_some_and(|opt| opt.remove_duplicates) {
        for group in &mut new_fpl.playlistgroups {
            // `HashSet::insert`  returns true for first insert, otherweise false
            group.channels.retain(|item| duplicates.insert(duplicate_hash(item)));
        }
    }

    for f in pipe {
        if let Some(groups) = f(&mut new_fpl.playlistgroups, target) {
            new_fpl.playlistgroups = groups;
        }
    }
    new_fpl
}

// This method is needed, because of duplicate group names in different inputs.
// We merge the same group names considering cluster together.
fn flatten_groups(playlistgroups: Vec<PlaylistGroup>) -> Vec<PlaylistGroup> {
    let mut sort_order: Vec<PlaylistGroup> = vec![];
    let mut idx: usize = 0;
    let mut group_map: HashMap<(String, XtreamCluster), usize> = HashMap::new();
    for group in playlistgroups {
        let key = (group.title.to_string(), group.xtream_cluster);
        match group_map.entry(key) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(idx);
                idx += 1;
                sort_order.push(group);
            }
            std::collections::hash_map::Entry::Occupied(o) => {
                sort_order.get_mut(*o.get()).unwrap().channels.extend(group.channels);
            }
        }
    }
    sort_order
}

async fn process_playlist_for_target(client: Arc<reqwest::Client>,
                                     playlists: &mut [FetchedPlaylist<'_>],
                                     target: &ConfigTarget,
                                     cfg: &Config,
                                     stats: &mut HashMap<String, InputStats>,
                                     errors: &mut Vec<TuliproxError>) -> Result<(), Vec<TuliproxError>> {
    let pipe = get_processing_pipe(target);
    debug_if_enabled!("Processing order is {}", &target.processing_order);

    let mut duplicates: HashSet<UUIDType> = HashSet::new();
    let mut processed_fetched_playlists: Vec<FetchedPlaylist> = vec![];

    debug!("Executing processing pipes");

    let mut step = StepMeasure::new("Pipes processed");
    for provider_fpl in playlists.iter_mut() {
        let mut processed_fpl = execute_pipe(target, &pipe, provider_fpl, &mut duplicates);
        playlist_resolve_series(Arc::clone(&client), cfg, target, errors, &pipe, provider_fpl, &mut processed_fpl).await;
        playlist_resolve_vod(Arc::clone(&client), cfg, target, errors, &mut processed_fpl).await;
        // stats
        let input_stats = stats.get_mut(&processed_fpl.input.name);
        if let Some(stat) = input_stats {
            stat.processed_stats.group_count = processed_fpl.playlistgroups.len();
            stat.processed_stats.channel_count = processed_fpl.playlistgroups.iter()
                .map(|group| group.channels.len())
                .sum();
        }
        processed_fetched_playlists.push(processed_fpl);
    }

    step.tick("Processed epg");
    let (new_epg, new_playlist) = process_epg(&mut processed_fetched_playlists);

    if new_playlist.is_empty() {
        info!("Playlist is empty: {}", &target.name);
        Ok(())
    } else {
        step.tick("Merged playlists");
        let mut flat_new_playlist = flatten_groups(new_playlist);
        step.tick("Sorted playlists");
        sort_playlist(target, &mut flat_new_playlist);
        step.tick("Assigned channel number");
        assign_channel_no_playlist(&mut flat_new_playlist);
        step.tick("Assigned channel counter");
        map_playlist_counter(target, &mut flat_new_playlist);
        step.tick("Processed group watches");
        process_watch(&client, target, cfg, &flat_new_playlist);
        step.tick("Persisting playlists");
        let result = persist_playlist(&mut flat_new_playlist, flatten_tvguide(&new_epg).as_ref(), target, cfg).await;
        step.stop();
        result
    }
}

fn process_epg(processed_fetched_playlists: &mut Vec<FetchedPlaylist>) -> (Vec<Epg>, Vec<PlaylistGroup>) {
    let mut new_playlist = vec![];
    let mut new_epg = vec![];

    // each fetched playlist can have its own epgl url.
    // we need to process each input epg.
    for fp in processed_fetched_playlists {
        process_playlist_epg(fp, &mut new_epg);
        new_playlist.append(&mut fp.playlistgroups);
    }
    (new_epg, new_playlist)
}

fn process_watch(client: &Arc<reqwest::Client>, target: &ConfigTarget, cfg: &Config, new_playlist: &Vec<PlaylistGroup>) {
    if target.t_watch_re.is_some() {
        if default_as_default().eq_ignore_ascii_case(&target.name) {
            error!("cant watch a target with no unique name");
        } else {
            let watch_re = target.t_watch_re.as_ref().unwrap();
            for pl in new_playlist {
                if watch_re.iter().any(|r| r.is_match(&pl.title)) {
                    process_group_watch(client, cfg, &target.name, pl);
                }
            }
        }
    }
}

pub async fn exec_processing(client: Arc<reqwest::Client>, cfg: Arc<Config>, targets: Arc<ProcessTargets>) {
    let start_time = Instant::now();
    let (stats, errors) = process_sources(Arc::clone(&client), cfg.clone(), targets.clone()).await;
    // log errors
    for err in &errors {
        error!("{}", err.message);
    }
    if let Ok(stats_msg) = serde_json::to_string(&serde_json::Value::Object(serde_json::map::Map::from_iter([("stats".to_string(), serde_json::to_value(stats).unwrap())]))) {
        // print stats
        info!("{stats_msg}");
        // send stats
        send_message(&client, &MsgKind::Stats, cfg.messaging.as_ref(), stats_msg.as_str());
    }
    // send errors
    if let Some(message) = get_errors_notify_message!(errors, 255) {
        if let Ok(error_msg) = serde_json::to_string(&serde_json::Value::Object(serde_json::map::Map::from_iter([("errors".to_string(), serde_json::Value::String(message))]))) {
            send_message(&client, &MsgKind::Error, cfg.messaging.as_ref(), error_msg.as_str());
        }
    }
    let elapsed = start_time.elapsed().as_secs();
    info!("🌷 Update process finished! Took {elapsed} secs.");
}

#[cfg(test)]
mod tests {
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

}