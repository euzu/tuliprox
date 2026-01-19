use crate::api::model::AppState;
use crate::messaging::send_message;
use crate::model::{is_input_expired, xtream_mapping_option_from_target_options, AppConfig, Config, ConfigInput, ConfigTarget, XtreamLoginInfo, XtreamTargetOutput};
use crate::model::{InputSource, ProxyUserCredentials};
use crate::processing::parser::xtream;
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::repository::bplustree::BPlusTreeUpdate;
use crate::repository::playlist_repository::{get_target_id_mapping, rewrite_provider_series_info_episode_virtual_id, ProviderEpisodeKey};
use crate::repository::storage::{ensure_input_storage_path, get_input_storage_path, get_target_storage_path};
use crate::repository::target_id_mapping::VirtualIdRecord;
use crate::repository::xtream_repository::{get_live_cat_collection_path, get_series_cat_collection_path, get_vod_cat_collection_path, xtream_get_file_path, CategoryEntry};
use crate::repository::xtream_repository::{persist_input_vod_info, persists_input_series_info, write_playlist_batch_item_upsert, write_playlist_item_update};
use crate::utils::request;
use chrono::{DateTime, Utc};
use log::{error, info, warn};
use shared::error::{TuliproxError};
use shared::model::{MsgKind, PlaylistEntry, PlaylistGroup, ProxyUserStatus, SeriesStreamProperties, StreamProperties, VideoStreamProperties, XtreamCluster, XtreamPlaylistItem, XtreamSeriesInfo, XtreamVideoInfo, XtreamVideoInfoDoc};
use shared::utils::{extract_extension_from_url, get_i64_from_serde_value, get_string_from_serde_value, sanitize_sensitive_info, Internable};
use std::collections::HashMap;
use std::io::Error;
use std::path::Path;
use std::str::FromStr;

use crate::model::XtreamCategory;
use crate::utils::request::DynReader;
use fs2::FileExt;
use std::fs::File;
use std::sync::Arc;

use shared::{info_err, notify_err, notify_err_res};

const THREE_DAYS_IN_SECS: i64 = 3 * 24 * 60 * 60;

#[inline]
pub fn get_xtream_stream_url_base(url: &str, username: &str, password: &str) -> String {
    format!("{url}/player_api.php?username={username}&password={password}")
}

pub fn get_xtream_player_api_action_url(input: &ConfigInput, action: &str) -> Option<String> {
    if let Some(user_info) = input.get_user_info() {
        Some(format!("{}&action={}",
                     get_xtream_stream_url_base(
                         &user_info.base_url,
                         &user_info.username,
                         &user_info.password),
                     action
        ))
    } else {
        None
    }
}

pub fn get_xtream_player_api_info_url(input: &ConfigInput, cluster: XtreamCluster, stream_id: u32) -> Option<String> {
    let (action, stream_id_field) = match cluster {
        XtreamCluster::Live => (crate::model::XC_ACTION_GET_LIVE_INFO, crate::model::XC_LIVE_ID),
        XtreamCluster::Video => (crate::model::XC_ACTION_GET_VOD_INFO, crate::model::XC_VOO_ID),
        XtreamCluster::Series => (crate::model::XC_ACTION_GET_SERIES_INFO, crate::model::XC_SERIES_ID),
    };
    get_xtream_player_api_action_url(input, action).map(|action_url| format!("{action_url}&{stream_id_field}={stream_id}"))
}


pub async fn get_xtream_stream_info_content(client: &reqwest::Client, input: &InputSource, trace_log: bool) -> Result<String, Error> {
    match request::download_text_content(client, None, input, None, None, trace_log).await {
        Ok((content, _response_url)) => Ok(content),
        Err(err) => Err(err)
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn get_xtream_stream_info(client: &reqwest::Client,
                                    app_state: &Arc<AppState>,
                                    user: &ProxyUserCredentials,
                                    input: &ConfigInput,
                                    target: &ConfigTarget,
                                    pli: &XtreamPlaylistItem,
                                    info_url: &str,
                                    cluster: XtreamCluster) -> Result<String, TuliproxError> {
    let xtream_output = target.get_xtream_output().ok_or_else(|| info_err!("Unexpected error, missing xtream output"))?;

    let app_config = &app_state.app_config;
    let server_info = app_config.get_user_server_info(user);
    let options = xtream_mapping_option_from_target_options(target, xtream_output, app_config, user, Some(server_info.get_base_url().as_str()));

    if let Some(content) = pli.get_resolved_info_document(&options) {
        return serde_json::to_string(&content).map_err(|err| info_err!("{err}"));
    }

    let input_source = InputSource::from(input).with_url(info_url.to_owned());
    if let Ok(content) = get_xtream_stream_info_content(client, &input_source, false).await {
        if content.is_empty() {
            return Err(info_err!("Provider returned no response for stream with id: {}/{}/{}",
                                                  target.name.replace(' ', "_").as_str(), &cluster, pli.get_virtual_id()));
        }
        if let Some(provider_id) = pli.get_provider_id() {
            match cluster {
                XtreamCluster::Live => {}
                XtreamCluster::Video => {
                    let working_dir = &app_config.config.load().working_dir;
                    if let Ok(storage_path) = get_input_storage_path(&input.name, working_dir) {
                        match serde_json::from_str::<XtreamVideoInfo>(&content) {
                            Ok(info) => {
                                // parse downloaded info into StreamProperties
                                let video_stream_props = VideoStreamProperties::from_info(&info, pli);

                                // persist input info
                                if let Err(err) = persist_input_vod_info(&app_state.app_config, &storage_path, cluster, &input.name, provider_id, &video_stream_props).await {
                                    error!("Failed to persist video stream for input {}: {err}", &input.name);
                                }

                                // update target playlist
                                let mut vod_pli = pli.clone();
                                vod_pli.additional_properties = Some(StreamProperties::Video(Box::new(video_stream_props)));

                                if let Err(err) = write_playlist_item_update(app_config, &target.name, &vod_pli).await {
                                    error!("Failed to persist video stream: {err}");
                                }

                                if target.use_memory_cache {
                                    app_state.playlists.update_playlist_items(target, vec![&vod_pli]).await;
                                }

                                if let Some(value) = xtream_resolve_stream_info(app_state, user, target, xtream_output, &vod_pli) {
                                    return value;
                                }
                            }
                            Err(err) => error!("Failed to persist video info: {err}")
                        }
                    }
                }
                XtreamCluster::Series => {
                    let working_dir = &app_config.config.load().working_dir;
                    let group = pli.get_group();
                    let series_name = pli.get_name();

                    match serde_json::from_str::<XtreamSeriesInfo>(&content) {
                        Ok(info) => {
                            // parse series info
                            let series_stream_props = SeriesStreamProperties::from_info(&info, pli);
                            
                            if let Ok(storage_path) = get_input_storage_path(&input.name, working_dir) {
                                // update input db
                                if let Err(err) = persists_input_series_info(app_config, &storage_path, cluster, &input.name, provider_id, &series_stream_props).await {
                                    error!("Failed to persist series info for input {}: {err}", &input.name);
                                }
                            }
                            if let Some(mut episodes) = parse_xtream_series_info(&pli.get_uuid(), &series_stream_props, &group, &series_name, input) {
                                let config = &app_state.app_config.config.load();
                                match get_target_storage_path(config, target.name.as_str()) {
                                    None => {
                                        error!("Failed to get target storage path {}. Can't save episodes", &target.name);
                                    }
                                    Some(target_path) => {
                                        let mut in_memory_updates = Vec::new();
                                        let mut provider_series: HashMap<Arc<str>, Vec<ProviderEpisodeKey>> = HashMap::new();
                                        {
                                            let (mut target_id_mapping, _file_lock) = get_target_id_mapping(&app_state.app_config, &target_path, target.use_memory_cache).await?;
                                            
                                            if let Some(parent_id) = pli.get_provider_id() {
                                                let category_id = pli.get_category_id().unwrap_or(0);
                                                for episode in &mut episodes {
                                                    episode.header.virtual_id = target_id_mapping.get_and_update_virtual_id(&episode.header.uuid, provider_id, episode.header.item_type, parent_id);
                                                    episode.header.category_id = category_id;
                                                    let episode_provider_id = episode.header.get_provider_id().unwrap_or(0);
                                                    provider_series.entry(pli.get_uuid().intern())
                                                        .or_default()
                                                        .push(ProviderEpisodeKey {
                                                            provider_id: episode_provider_id,
                                                            virtual_id: episode.header.virtual_id,
                                                        });
                                                    if target.use_memory_cache {
                                                        in_memory_updates.push(
                                                            VirtualIdRecord::new(
                                                                episode.header.get_provider_id().unwrap_or(0),
                                                                episode.header.virtual_id,
                                                                episode.header.item_type,
                                                                provider_id,
                                                                episode.get_uuid(),
                                                            ),
                                                        );
                                                    }
                                                }
                                            }
                                            if let Err(err) = target_id_mapping.persist() {
                                                error!("Failed to persist target id mapping: {err}");
                                            }
                                        }

                                        let xtream_episodes: Vec<XtreamPlaylistItem> = episodes.iter().map(XtreamPlaylistItem::from).collect();
                                        if let Err(err) = write_playlist_batch_item_upsert(
                                            app_config,
                                            &target.name,
                                            XtreamCluster::Series,
                                            &xtream_episodes).await {
                                            error!("Failed to persist playlist batch item update: {err}");
                                        }

                                        if target.use_memory_cache && !in_memory_updates.is_empty() {
                                            app_state.playlists.insert_playlist_items(target, episodes).await;
                                            app_state.playlists.update_target_id_mapping(target, in_memory_updates).await;
                                        }

                                        if !provider_series.is_empty() {
                                            let mut series_pli = pli.clone();
                                            series_pli.additional_properties = Some(StreamProperties::Series(Box::new(series_stream_props)));
                                            rewrite_provider_series_info_episode_virtual_id(&mut series_pli, &provider_series);
                                            if let Err(err) = write_playlist_item_update(app_config, &target.name, &series_pli).await {
                                                error!("Failed to persist series stream: {err}");
                                            }
                                            app_state.playlists.update_playlist_items(target, vec![&series_pli]).await;

                                            if let Some(value) = xtream_resolve_stream_info(app_state, user, target, xtream_output, &series_pli) {
                                                return value;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            error!("Failed to persist series info: {err}");
                        }
                    }
                }
            }
        }
    }

    Err(info_err!("Can't find stream with id: {}/{}/{}",
                                   target.name.replace(' ', "_").as_str(), &cluster, pli.get_virtual_id()))
}

fn xtream_resolve_stream_info(app_state: &Arc<AppState>, user: &ProxyUserCredentials,
                              target: &ConfigTarget, xtream_output: &XtreamTargetOutput,
                              pli: &XtreamPlaylistItem) -> Option<Result<String, TuliproxError>> {
    let app_config = &app_state.app_config;
    let server_info = app_config.get_user_server_info(user);
    let options = xtream_mapping_option_from_target_options(target, xtream_output, app_config, user, Some(server_info.get_base_url().as_str()));
    if let Some(content) = pli.get_resolved_info_document(&options) {
        return Some(serde_json::to_string(&content).map_err(|err| info_err!("Failed to serialize stream info: {err}")));
    }
    None
}

fn get_skip_cluster(input: &ConfigInput) -> Vec<XtreamCluster> {
    let mut skip_cluster = vec![];
    if let Some(input_options) = &input.options {
        if input_options.xtream_skip_live {
            skip_cluster.push(XtreamCluster::Live);
        }
        if input_options.xtream_skip_vod {
            skip_cluster.push(XtreamCluster::Video);
        }
        if input_options.xtream_skip_series {
            skip_cluster.push(XtreamCluster::Series);
        }
    }
    if skip_cluster.len() == 3 {
        info!("You have skipped all sections from xtream input {}", &input.name);
    }
    skip_cluster
}

const ACTIONS: [(XtreamCluster, &str, &str); 3] = [
    (XtreamCluster::Live, crate::model::XC_ACTION_GET_LIVE_CATEGORIES, crate::model::XC_ACTION_GET_LIVE_STREAMS),
    (XtreamCluster::Video, crate::model::XC_ACTION_GET_VOD_CATEGORIES, crate::model::XC_ACTION_GET_VOD_STREAMS),
    (XtreamCluster::Series, crate::model::XC_ACTION_GET_SERIES_CATEGORIES, crate::model::XC_ACTION_GET_SERIES)];

async fn xtream_login(cfg: &Config, client: &reqwest::Client, input: &InputSource, username: &str) -> Result<Option<XtreamLoginInfo>, TuliproxError> {
    let content = if let Ok(content) = request::get_input_json_content(client, None, input, None, false).await {
        content
    } else {
        let input_source_account_info =
            input.with_url(format!("{}&action={}", &input.url, crate::model::XC_ACTION_GET_ACCOUNT_INFO));
        match request::get_input_json_content(client, None, &input_source_account_info, None, false).await {
            Ok(content) => content,
            Err(err) => {
                warn!("Failed to login xtream account {username} {err}");
                return Err(err);
            }
        }
    };

    let mut login_info = XtreamLoginInfo {
        status: None,
        exp_date: None,
    };

    if let Some(user_info) = content.get("user_info") {
        if let Some(status_value) = user_info.get("status") {
            if let Some(status) = get_string_from_serde_value(status_value) {
                if let Ok(cur_status) = ProxyUserStatus::from_str(&status) {
                    login_info.status = Some(cur_status);
                    if !matches!(cur_status, ProxyUserStatus::Active | ProxyUserStatus::Trial) {
                        warn!("User status for user {username} is {cur_status:?}");
                        send_message(client, MsgKind::Info, cfg.messaging.as_ref(),
                                     &format!("User status for user {username} is {cur_status:?}")).await;
                    }
                }
            }
        }

        if let Some(exp_value) = user_info.get("exp_date") {
            if let Some(expiration_timestamp) = get_i64_from_serde_value(exp_value) {
                login_info.exp_date = Some(expiration_timestamp);
                notify_account_expire(login_info.exp_date, cfg, client, username, &input.name).await;
            }
        }
    }

    if login_info.exp_date.is_none() && login_info.status.is_none() {
        Ok(None)
    } else {
        Ok(Some(login_info))
    }
}

pub async fn notify_account_expire(exp_date: Option<i64>, cfg: &Config, client: &reqwest::Client,
                                   username: &str, input_name: &str) {
    if let Some(expiration_timestamp) = exp_date {
        let now_secs = Utc::now().timestamp(); // UTC-Time
        if expiration_timestamp > now_secs {
            let time_left = expiration_timestamp - now_secs;

            if time_left < THREE_DAYS_IN_SECS {
                if let Some(datetime) = DateTime::<Utc>::from_timestamp(expiration_timestamp, 0) {
                    let formatted = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
                    warn!("User account for user {username} expires {formatted}");
                    send_message(client, MsgKind::Info, cfg.messaging.as_ref(),
                                 &format!("User account for user {username} expires {formatted}")).await;
                }
            }
        } else {
            warn!("User account for user {username} is expired");
            send_message(client, MsgKind::Info, cfg.messaging.as_ref(),
                         &format!("User account for user {username} for provider {input_name} is expired")).await;
        }
    }
}

pub async fn download_xtream_playlist(app_config: &Arc<AppConfig>, client: &reqwest::Client, input: &ConfigInput, clusters: Option<&[XtreamCluster]>)
                                      -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool) {
    let cfg = app_config.config.load();
    let input_source: InputSource = {
        match input.staged.as_ref() {
            None => input.into(),
            Some(staged) => staged.into(),
        }
    };

    let username = input_source.username.as_ref().map_or("", |v| v);
    let password = input_source.password.as_ref().map_or("", |v| v);

    let base_url = get_xtream_stream_url_base(&input_source.url, username, password);
    let input_source_login = input_source.with_url(base_url.clone());

    check_alias_user_state(&cfg, client, input).await;

    if let Err(err) = xtream_login(&cfg, client, &input_source_login, username).await {
        error!("Could not log in with xtream user {username} for provider {}. {err}", input.name);
        return (Vec::with_capacity(0), vec![err], false);
    }

    let mut playlist_groups: Vec<PlaylistGroup> = Vec::with_capacity(128);
    let skip_cluster = get_skip_cluster(input);

    let working_dir = &cfg.working_dir;

    let mut errors = vec![];
    for (xtream_cluster, category, stream) in &ACTIONS {
        let is_requested = clusters.is_none_or(|c| c.contains(xtream_cluster));
        if is_requested && !skip_cluster.contains(xtream_cluster) {
            let input_source_category = input_source.with_url(format!("{base_url}&action={category}"));
            let input_source_stream = input_source.with_url(format!("{base_url}&action={stream}"));
            let category_file_path = crate::utils::prepare_file_path(input.persist.as_deref(),
                                                                     working_dir, format!("{category}_").as_str());
            let stream_file_path = crate::utils::prepare_file_path(input.persist.as_deref(),
                                                                   working_dir, format!("{stream}_").as_str());

            match futures::join!(
                request::get_input_json_content_as_stream(client, None, &input_source_category, category_file_path),
                request::get_input_json_content_as_stream(client, None, &input_source_stream, stream_file_path)
            ) {
                (Ok(category_content), Ok(stream_content)) => {
                    if cfg.disk_based_processing {
                        // trace!("Using disk input playlist optimization for cluster {}", xtream_cluster);
                        if let Err(err) = process_xtream_cluster_to_disk(app_config, input, *xtream_cluster, category_content, stream_content).await {
                            error!("process_xtream_cluster_to_disk failed: {err}");
                            errors.push(err);
                        } else {
                            // trace!("process_xtream_cluster_to_disk succeeded for cluster {}", xtream_cluster);
                        }
                    } else {
                        // trace!("Using in-memory playlist parsing for cluster {}", xtream_cluster);
                        match xtream::parse_xtream(input,
                                                   *xtream_cluster,
                                                   category_content,
                                                   stream_content).await {
                            Ok(sub_playlist_parsed) => {
                                if let Some(mut xtream_sub_playlist) = sub_playlist_parsed {
                                    playlist_groups.append(&mut xtream_sub_playlist);
                                } else {
                                    error!("Could not parse playlist {xtream_cluster} for input {}: {}",
                                        input_source.name, sanitize_sensitive_info(&input_source.url));
                                }
                            }
                            Err(err) => errors.push(err)
                        }
                    }
                }
                (Err(err1), Err(err2)) => {
                    errors.extend([err1, err2]);
                }
                (_, Err(err)) | (Err(err), _) => errors.push(err),
            }
        }
    }

    for (grp_id, plg) in (1_u32..).zip(playlist_groups.iter_mut()) {
        plg.id = grp_id;
    }

    (playlist_groups, errors, cfg.disk_based_processing)
}

async fn check_alias_user_state(cfg: &Arc<Config>, client: &reqwest::Client, input: &ConfigInput) {
    if let Some(aliases) = input.aliases.as_ref() {
        for alias in aliases {
            if is_input_expired(alias.exp_date) {
                notify_account_expire(alias.exp_date, cfg, client, alias.username.as_ref()
                    .map_or("", |s| s.as_str()), &alias.name).await;
            }
        }
    }

    // TODO figure out how and when to call it to avoid provider bans. Possible reason for provider ban is to avoid brute force attacks.

    //
    // let cfg = Arc::clone(cfg);
    // let client = Arc::clone(client);
    // let input = Arc::clone(input);
    //
    // tokio::spawn(async move {
    //     for alias in &aliases {
    //         // Random wait time  60–180 seconds to avoid provider block
    //         let delay = u64::from(fastrand::u32(60..=180));
    //         tokio::time::sleep(tokio::time::Duration::from_secs(delay)).await;
    //
    //         if let (Some(username), Some(password)) =
    //             (alias.username.as_ref(), alias.password.as_ref())
    //         {
    //             let mut input_source: InputSource = input.as_ref().into();
    //             input_source.username.clone_from(&alias.username);
    //             input_source.password.clone_from(&alias.password);
    //             input_source.url.clone_from(&alias.url);
    //             let base_url = get_xtream_stream_url_base(
    //                 &input_source.url,
    //                 username,
    //                 password,
    //             );
    //             let input_source_login = input_source.with_url(base_url.clone());
    //
    //             match xtream_login(&cfg, &client, &input_source_login, username).await {
    //                 Ok(Some(xtream_login_info)) => {
    //                     // TODO need to update the alias
    //
    //                 }
    //                 Ok(None) => error!("Could log in with xtream user {} for provider {}. But could not extract account info", username, alias.name),
    //                 Err(err) => error!("Could not log in with xtream user {} for provider {}. {err}",username,alias.name),
    //             }
    //         }
    //     }
    // });
}

pub fn create_vod_info_from_item(target: &ConfigTarget, user: &ProxyUserCredentials, pli: &XtreamPlaylistItem) -> String {
    let category_id = pli.category_id;
    let stream_id = if user.proxy.is_redirect(pli.item_type) || target.is_force_redirect(pli.item_type) { pli.provider_id } else { pli.virtual_id };
    let added = pli.additional_properties.as_ref().and_then(StreamProperties::get_last_modified).unwrap_or(0);
    let name = &pli.name;
    let extension = pli
        .get_container_extension()
        .as_deref()
        .filter(|ce| !ce.is_empty())
        .or_else(|| extract_extension_from_url(&pli.url))
        .map_or_else(String::new, ToString::to_string);

    let mut doc = XtreamVideoInfoDoc::default();
    doc.info.name.clone_from(name);
    doc.movie_data.stream_id = stream_id;
    doc.movie_data.name.clone_from(name);
    doc.movie_data.added = added.intern();
    doc.movie_data.category_id = category_id.intern();
    doc.movie_data.category_ids.push(category_id);
    doc.movie_data.container_extension = extension.intern();
    doc.movie_data.custom_sid = None;

    serde_json::to_string(&doc).unwrap_or(String::new())
}

const BATCH_SIZE: usize = 1000;

async fn process_xtream_cluster_to_disk(
    app_config: &Arc<AppConfig>,
    input: &ConfigInput,
    cluster: XtreamCluster,
    categories: DynReader,
    streams: DynReader,
) -> Result<(), TuliproxError> {
    let cfg = app_config.config.load();
    // trace!("Starting process_xtream_cluster_to_disk for cluster {}", cluster);
    let storage_path = {
        ensure_input_storage_path(&cfg, &input.name)?
    };
    let xtream_path = xtream_get_file_path(&storage_path, cluster);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<XtreamPlaylistItem>(BATCH_SIZE * 2);
    let input_clone = input.clone();
    let parse_task = tokio::spawn(async move {
        // trace!("Spawned parse_task for cluster {}", cluster);
        xtream::parse_xtream_streaming(&input_clone, cluster, categories, streams, move |item| {
            // trace!("Parsed item {}: {}", item.virtual_id, item.name);
            if tx.blocking_send(item).is_err() {
                return notify_err_res!("Channel closed while processing {cluster}");
            }
            Ok(())
        }).await
    });

    let xtream_path_for_consumer = xtream_path.clone();
    let consumer_task = tokio::task::spawn_blocking(move || {
        // trace!("Spawned consumer_task for cluster {}", cluster);
        let tmp_xtream_path = xtream_path_for_consumer.with_extension("tmp");
        // trace!("Creating fresh ghost database at {:?}", tmp_xtream_path);
        crate::repository::bplustree::BPlusTree::<u32, XtreamPlaylistItem>::new()
            .store(&tmp_xtream_path)
            .map_err(|e| {
                error!("Failed to initialize ghost BPlusTree at {}: {e}", tmp_xtream_path.display());
                notify_err!("Init tree error {e}")
            })?;

        let mut tree = BPlusTreeUpdate::try_new(&tmp_xtream_path)
            .map_err(|e| {
                error!("Failed to open ghost tree at {}: {e}", tmp_xtream_path.display());
                notify_err!("Failed to open tree {e}")
            })?;

        let mut buffer = Vec::with_capacity(BATCH_SIZE);
        // let mut total_items = 0;

        while let Some(item) = rx.blocking_recv() {
            buffer.push(item);
            // total_items += 1;
            if buffer.len() >= BATCH_SIZE {
                let batch: Vec<(&u32, &XtreamPlaylistItem)> = buffer.iter().map(|i| (&i.provider_id, i)).collect();
                tree.upsert_batch(&batch).map_err(|e| {
                    error!("Batch upsert failed for cluster {cluster}: {e}");
                    notify_err!("Upsert failed {e}")
                })?;
                buffer.clear();
            }
        }

        // trace!("Finished receiving items for {}, total: {}", cluster, total_items);
        if !buffer.is_empty() {
            // trace!("Writing final batch of {} items to disk for cluster {}", buffer.len(), cluster);
            let batch: Vec<(&u32, &XtreamPlaylistItem)> = buffer.iter().map(|i| (&i.provider_id, i)).collect();
            tree.upsert_batch(&batch).map_err(|e| {
                error!("Final batch upsert failed for cluster {cluster}: {e}");
                notify_err!("Upsert failed {e}")
            })?;
        }
        Ok::<(), TuliproxError>(())
    });

    let (parse_res, consumer_res) = futures::join!(parse_task, consumer_task);
    // trace!("Joined tasks for cluster {}", cluster);

    let categories = parse_res.map_err(|e| notify_err!("Parse task join err {e}"))??;
    consumer_res.map_err(|e| notify_err!("Consumer task join err {e}"))??;

    // 1. Save categories to a temporary file
    let col_path = match cluster {
        XtreamCluster::Live => get_live_cat_collection_path(&storage_path),
        XtreamCluster::Video => get_vod_cat_collection_path(&storage_path),
        XtreamCluster::Series => get_series_cat_collection_path(&storage_path),
    };
    let tmp_col_path = col_path.with_extension("tmp");
    save_xtream_categories_to_file(&tmp_col_path, &categories).await?;

    // 2. Success! Swap temporary files to permanent ones
    let tmp_xtream_path = xtream_path.with_extension("tmp");

    // Acquire write lock to serialize compact/swap/cleanup operations across concurrent API calls
    let swap_lock = app_config.file_locks.write_lock(&xtream_path).await;

    if let Ok(mut tree_update) = BPlusTreeUpdate::<u32, XtreamPlaylistItem>::try_new(&tmp_xtream_path) {
        // Compact the TEMPORARY file (tmp_xtream_path) in place.
        // We ensure the .tmp file is compacted before we rename it to the final destination,
        // so that the final database file is fresh and optimized.
        if let Err(e) = tree_update.compact(&tmp_xtream_path) {
            error!("Failed to compact temporary database for {cluster}: {e}");
            // We continue anyway, as uncompacted data is better than no data.
        }
    }

    // trace!("Performing atomic swap for cluster {}", cluster);
    if let Err(e) = crate::utils::rename_or_copy(&tmp_xtream_path, &xtream_path, false) {
        error!("Failed to swap xtream database for {cluster}: {e}");
        return notify_err_res!("Failed to swap database: {e}");
    }

    if let Err(e) = crate::utils::rename_or_copy(&tmp_col_path, &col_path, false) {
        error!("Failed to swap xtream categories for {cluster}: {e}");
        return notify_err_res!("Failed to swap categories: {e}");
    }

    // Cleanup - temporary files are usually replaced/moved by swap, but defensive removal of leftovers
    // We strictly check for existence first to avoid errors if rename_or_copy acted as a move.
    if tokio::fs::try_exists(&tmp_xtream_path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(tmp_xtream_path).await;
    }
    if tokio::fs::try_exists(&tmp_col_path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(tmp_col_path).await;
    }

    drop(swap_lock);
    // trace!("Cluster {} updated successfully", cluster);
    Ok(())
}

async fn save_xtream_categories_to_file(col_path: &Path, categories: &[XtreamCategory]) -> Result<(), TuliproxError> {
    let col_path_buf = col_path.to_path_buf();
    let cat_entries: Vec<CategoryEntry> = categories.iter().map(|c| CategoryEntry {
        category_id: c.category_id,
        category_name: c.category_name.clone(),
        parent_id: 0,
    }).collect();

    tokio::task::spawn_blocking(move || {
        if let Ok(file) = File::create(&col_path_buf) {
            if let Err(e) = file.lock_exclusive() {
                warn!("Could not acquire exclusive lock for {}: {e}, proceeding without lock", col_path_buf.display());
            }
            serde_json::to_writer(&file, &cat_entries).map_err(|e| {
                error!("Failed to write categories to file {}: {e}", col_path_buf.display());
                notify_err!("Write failed: {e}")
            })?;
            let _ = file.unlock();
        } else {
            return notify_err_res!("Failed to create category file {}", col_path_buf.display());
        }
        Ok(())
    }).await.map_err(|e| notify_err!("Spawn error {e}"))?
}
