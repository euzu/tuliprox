use crate::api::model::AppState;
use crate::messaging::send_message;
use crate::model::{is_input_expired, xtream_mapping_option_from_target_options, AppConfig,
                   ConfigInput, ConfigInputFlags, ConfigTarget, MessageContent, XtreamLoginInfo, XtreamTargetOutput};
use crate::model::{InputSource, ProxyUserCredentials};
use shared::model::ClusterSource;
use crate::processing::parser::xtream;
use crate::processing::parser::xtream::parse_xtream_series_info;
use crate::repository::VirtualIdRecord;
use crate::repository::{get_input_storage_path, get_target_storage_path};
use crate::repository::{get_target_id_mapping, rewrite_provider_series_info_episode_virtual_id, ProviderEpisodeKey};
use crate::repository::{persist_input_xtream_playlist_cluster_to_disk, persist_input_vod_info, persists_input_series_info, write_playlist_batch_item_upsert, write_playlist_item_update};
use crate::utils::request;
use chrono::{DateTime, Utc};
use log::{error, info, warn};
use shared::error::TuliproxError;
use shared::model::{PlaylistEntry, PlaylistGroup, ProxyUserStatus, SeriesStreamProperties,
                    StreamProperties, VideoStreamProperties, InputType, XtreamCluster, XtreamPlaylistItem,
                    XtreamSeriesInfo, XtreamVideoInfo, XtreamVideoInfoDoc};
use shared::utils::{extract_extension_from_url, get_i64_from_serde_value, get_string_from_serde_value, sanitize_sensitive_info, Internable, PROVIDER_SCHEME_PREFIX};
use std::collections::HashMap;
use std::io::Error;
use std::str::FromStr;

use std::sync::Arc;

use shared::{concat_string, info_err};

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


pub async fn get_xtream_stream_info_content(app_config: &Arc<AppConfig>, client: &reqwest::Client, input: &InputSource, trace_log: bool) -> Result<String, Error> {
    match request::download_text_content(app_config, client, input, None, None, trace_log).await {
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
    let encrypt_secret = app_state.get_encrypt_secret();
    let options = xtream_mapping_option_from_target_options(target, xtream_output, app_config, user, encrypt_secret);

    if let Some(content) = pli.get_resolved_info_document(&options) {
        return serde_json::to_string(&content).map_err(|err| info_err!("{err}"));
    }

    let resolved_url = input.resolve_url(info_url)?;
    let input_source = InputSource::from(input).with_url(resolved_url.to_string());
    if let Ok(content) = get_xtream_stream_info_content(app_config, client, &input_source, false).await {
        if content.is_empty() {
            return Err(info_err!("Provider returned no response for stream with id: {}/{}/{}",
                                                  target.name.replace(' ', "_").as_str(), &cluster, pli.get_virtual_id()));
        }
        if let Some(provider_id) = pli.get_provider_id() {
            match cluster {
                XtreamCluster::Live => {}
                XtreamCluster::Video => {
                    let storage_dir = &app_config.config.load().storage_dir;
                    if let Ok(storage_path) = get_input_storage_path(&input.name, storage_dir).await {
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
                    let storage_dir = &app_config.config.load().storage_dir;
                    let group = pli.get_group();
                    let series_name = pli.get_name();

                    match serde_json::from_str::<XtreamSeriesInfo>(&content) {
                        Ok(info) => {
                            // parse series info
                            let series_stream_props = SeriesStreamProperties::from_info(&info, pli);

                            if let Ok(storage_path) = get_input_storage_path(&input.name, storage_dir).await {
                                // update input db
                                if let Err(err) = persists_input_series_info(app_config, &storage_path, cluster, &input.name, provider_id, &series_stream_props).await {
                                    error!("Failed to persist series info for input {}: {err}", &input.name);
                                }
                            }

                            // Capture release date for children
                            let series_release_date = series_stream_props.release_date.clone();

                            if let Some(mut episodes) = parse_xtream_series_info(
                                &pli.get_uuid(),
                                &series_stream_props,
                                &group,
                                &series_name,
                                input,
                                series_release_date.as_ref(),
                                // `pli` is `XtreamPlaylistItem`, which stores header fields flattened.
                                // `source_ordinal` is copied from `PlaylistItemHeader.source_ordinal` on conversion.
                                pli.source_ordinal,
                            ) {
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

                                            if let Some(_parent_id) = pli.get_provider_id() {
                                                let category_id = pli.get_category_id().unwrap_or(0);
                                                for episode in &mut episodes {
                                                    let episode_provider_id = episode.header.get_provider_id().unwrap_or(0);
                                                    episode.header.virtual_id = target_id_mapping.get_and_update_virtual_id(&episode.header.uuid, episode_provider_id, episode.header.item_type, pli.virtual_id);
                                                    episode.header.category_id = category_id;
                                                    provider_series.entry(pli.get_uuid().intern())
                                                        .or_default()
                                                        .push(ProviderEpisodeKey {
                                                            provider_id: episode_provider_id,
                                                            virtual_id: episode.header.virtual_id,
                                                        });
                                                    if target.use_memory_cache {
                                                        in_memory_updates.push(
                                                            VirtualIdRecord::new(
                                                                episode_provider_id,
                                                                episode.header.virtual_id,
                                                                episode.header.item_type,
                                                                pli.virtual_id,
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
    let encrypt_secret = app_state.get_encrypt_secret();
    let options = xtream_mapping_option_from_target_options(target, xtream_output, app_config, user, encrypt_secret);
    if let Some(content) = pli.get_resolved_info_document(&options) {
        return Some(serde_json::to_string(&content).map_err(|err| info_err!("Failed to serialize stream info: {err}")));
    }
    None
}

pub(crate) fn get_skip_cluster(input: &ConfigInput) -> Vec<XtreamCluster> {
    let mut skip_cluster = vec![];
    if input.has_flag(ConfigInputFlags::XtreamSkipLive) {
        skip_cluster.push(XtreamCluster::Live);
    }
    if input.has_flag(ConfigInputFlags::XtreamSkipVod) {
        skip_cluster.push(XtreamCluster::Video);
    }
    if input.has_flag(ConfigInputFlags::XtreamSkipSeries) {
        skip_cluster.push(XtreamCluster::Series);
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

async fn xtream_login(app_config: &Arc<AppConfig>, client: &reqwest::Client, input: &InputSource, username: &str) -> Result<Option<XtreamLoginInfo>, TuliproxError> {
    let content = if let Ok(content) = request::get_input_json_content(app_config, client, input, None, false).await {
        content
    } else {
        let input_source_account_info =
            input.with_url(format!("{}&action={}", &input.url, crate::model::XC_ACTION_GET_ACCOUNT_INFO));
        match request::get_input_json_content(app_config, client, &input_source_account_info, None, false).await {
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
                        send_message(app_config, client, MessageContent::Error(format!("User status for user {username} is {cur_status:?}"))).await;
                    }
                }
            }
        }

        if let Some(exp_value) = user_info.get("exp_date") {
            if let Some(expiration_timestamp) = get_i64_from_serde_value(exp_value) {
                login_info.exp_date = Some(expiration_timestamp);
                notify_account_expire(login_info.exp_date, app_config, client, username, &input.name).await;
            }
        }
    }

    if login_info.exp_date.is_none() && login_info.status.is_none() {
        Ok(None)
    } else {
        Ok(Some(login_info))
    }
}

pub async fn notify_account_expire(exp_date: Option<i64>, app_config: &Arc<AppConfig>, client: &reqwest::Client,
                                   username: &str, input_name: &str) {
    if let Some(expiration_timestamp) = exp_date {
        let now_secs = Utc::now().timestamp(); // UTC-Time
        if expiration_timestamp > now_secs {
            let time_left = expiration_timestamp - now_secs;

            if time_left < THREE_DAYS_IN_SECS {
                if let Some(datetime) = DateTime::<Utc>::from_timestamp(expiration_timestamp, 0) {
                    let formatted = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
                    warn!("User account for user {username} expires {formatted}");
                    send_message(app_config, client, MessageContent::Info(format!("User account for user {username} expires {formatted}"))).await;
                }
            }
        } else {
            warn!("User account for user {username} is expired");
            send_message(app_config, client, MessageContent::Info(
                format!("User account for user {username} for provider {input_name} is expired"))).await;
        }
    }
}

/// Partitions the requested clusters into staged and main groups based on
/// skip flags and per-cluster source configuration.
pub(crate) fn partition_clusters_by_source(
    input: &ConfigInput,
    requested: Option<&[XtreamCluster]>,
    skip_cluster: &[XtreamCluster],
) -> (Vec<XtreamCluster>, Vec<XtreamCluster>) {
    let mut staged_clusters = Vec::new();
    let mut main_clusters = Vec::new();

    let all_clusters = [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];
    for cluster in &all_clusters {
        let is_requested = requested.is_none_or(|c| c.contains(cluster));
        if !is_requested || skip_cluster.contains(cluster) {
            continue;
        }

        if let Some(staged) = input.staged.as_ref().filter(|s| s.enabled) {
            match staged.get_cluster_source(*cluster) {
                ClusterSource::Staged => {
                    // M3U staged inputs are handled by the M3U download path, never by Xtream API calls.
                    if staged.input_type.is_xtream() {
                        staged_clusters.push(*cluster);
                    }
                }
                ClusterSource::Input => main_clusters.push(*cluster),
                ClusterSource::Skip => {} // excluded
            }
        } else {
            main_clusters.push(*cluster);
        }
    }

    (staged_clusters, main_clusters)
}

/// Downloads xtream clusters from a single source (either main input or staged input).
async fn download_xtream_from_source(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &ConfigInput,
    input_source: &InputSource,
    source_input_type: InputType,
    clusters: &[XtreamCluster],
) -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool) {
    let (username, password) = (
        input_source.username.as_deref().unwrap_or(""),
        input_source.password.as_deref().unwrap_or(""),
    );

    let is_provider_url = input_source.url.starts_with(PROVIDER_SCHEME_PREFIX);
    let base_input_url = if is_provider_url {
        input_source.url.clone()
    } else {
        match input.resolve_url(&input_source.url) {
            Ok(url) => url.into_owned(),
            Err(err) => return (Vec::with_capacity(0), vec![err], false),
        }
    };

    let base_url = get_xtream_stream_url_base(&base_input_url, username, password);
    let input_source_login = input_source.with_url(base_url.clone());

    if let Err(err) = xtream_login(app_config, client, &input_source_login, username).await {
        error!("Could not log in with xtream user {username} for provider {}. {err}", input.name);
        return (Vec::with_capacity(0), vec![err], false);
    }

    let mut playlist_groups: Vec<PlaylistGroup> = Vec::with_capacity(128);

    let cfg = app_config.config.load();
    let storage_dir = &cfg.storage_dir;
    let use_disk_based_processing =
        cfg.disk_based_processing && matches!(source_input_type, InputType::Xtream | InputType::XtreamBatch);

    let mut errors = vec![];
    for (xtream_cluster, category, stream) in &ACTIONS {
        if !clusters.contains(xtream_cluster) {
            continue;
        }
        let input_source_category = input_source.with_url(concat_string!(&base_url, "&action=", category));
        let input_source_stream = input_source.with_url(concat_string!(&base_url, "&action=", stream));
        let category_file_path = crate::utils::prepare_file_path(input.persist.as_deref(),
                                                                 storage_dir, concat_string!(category, "_").as_str());
        let stream_file_path = crate::utils::prepare_file_path(input.persist.as_deref(),
                                                               storage_dir, concat_string!(stream, "_").as_str());

        match futures::join!(
            request::get_input_json_content_as_stream(app_config, client, &input_source_category, category_file_path),
            request::get_input_json_content_as_stream(app_config, client, &input_source_stream, stream_file_path)
        ) {
            (Ok(category_content), Ok(stream_content)) => {
                if use_disk_based_processing {
                    if let Err(err) = persist_input_xtream_playlist_cluster_to_disk(app_config, input, *xtream_cluster, category_content, stream_content).await {
                        error!("persist_input_xtream_playlist_cluster_to_disk failed: {err}");
                        errors.push(err);
                    }
                } else {
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

    (playlist_groups, errors, use_disk_based_processing)
}

pub async fn download_xtream_playlist(app_config: &Arc<AppConfig>, client: &reqwest::Client, input: &ConfigInput, clusters: Option<&[XtreamCluster]>)
                                      -> (Vec<PlaylistGroup>, Vec<TuliproxError>, bool) {
    let skip_cluster = get_skip_cluster(input);
    let (staged_clusters, main_clusters) = partition_clusters_by_source(input, clusters, &skip_cluster);

    let mut all_groups = Vec::with_capacity(128);
    let mut all_errors = Vec::new();
    let mut any_disk = false;

    if !staged_clusters.is_empty() {
        if let Some(staged) = input.staged.as_ref().filter(|s| s.enabled) {
            let source: InputSource = staged.into();
            let (g, e, d) =
                download_xtream_from_source(app_config, client, input, &source, staged.input_type, &staged_clusters)
                    .await;
            all_groups.extend(g);
            all_errors.extend(e);
            any_disk |= d;
        }
    }

    if !main_clusters.is_empty() {
        check_alias_user_state(app_config, client, input).await;
        let source: InputSource = input.into();
        let (g, e, d) =
            download_xtream_from_source(app_config, client, input, &source, input.input_type, &main_clusters).await;
        all_groups.extend(g);
        all_errors.extend(e);
        any_disk |= d;
    }

    for (grp_id, plg) in (1_u32..).zip(all_groups.iter_mut()) {
        plg.id = grp_id;
    }

    (all_groups, all_errors, any_disk)
}

async fn check_alias_user_state(app_config: &Arc<AppConfig>, client: &reqwest::Client, input: &ConfigInput) {
    if let Some(aliases) = input.aliases.as_ref() {
        for alias in aliases {
            if is_input_expired(alias.exp_date) {
                notify_account_expire(alias.exp_date, app_config, client, alias.username.as_ref()
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
    let extension: String = pli
        .get_container_extension()
        .filter(|ce| !ce.is_empty())
        .map(|s| s.to_string())
        .or_else(|| extract_extension_from_url(&pli.url))
        .unwrap_or_default();

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

#[cfg(test)]
mod tests {
    use super::partition_clusters_by_source;
    use crate::model::{ConfigInput, ConfigInputFlags, ConfigInputFlagsSet, ConfigInputOptions, StagedInput};
    use shared::model::{ClusterSource, InputType, XtreamCluster};
    use shared::utils::Internable;

    fn test_input_with_staged(staged: StagedInput) -> ConfigInput {
        ConfigInput {
            name: "test".intern(),
            input_type: InputType::Xtream,
            staged: Some(staged),
            ..ConfigInput::default()
        }
    }

    fn options_with_flags(flags: &[ConfigInputFlags]) -> ConfigInputOptions {
        let mut set = ConfigInputFlagsSet::new();
        for flag in flags {
            set.set(*flag);
        }
        ConfigInputOptions { flags: set, ..ConfigInputOptions::defaults().clone() }
    }

    #[test]
    fn partition_respects_skip_flags_before_cluster_source() {
        let staged = StagedInput {
            enabled: true,
            input_type: InputType::Xtream,
            live_source: ClusterSource::Staged,
            vod_source: ClusterSource::Input,
            series_source: ClusterSource::Skip,
            ..StagedInput::default()
        };
        let mut input = test_input_with_staged(staged);
        input.options = Some(options_with_flags(&[ConfigInputFlags::XtreamSkipLive]));

        let skip_cluster = super::get_skip_cluster(&input);
        let (staged_clusters, main_clusters) = partition_clusters_by_source(&input, None, &skip_cluster);

        assert!(staged_clusters.is_empty());
        assert_eq!(main_clusters, vec![XtreamCluster::Video]);
    }

    #[test]
    fn partition_excludes_staged_clusters_for_m3u_staged_inputs() {
        let staged = StagedInput {
            enabled: true,
            input_type: InputType::M3u,
            live_source: ClusterSource::Staged,
            vod_source: ClusterSource::Input,
            series_source: ClusterSource::Input,
            ..StagedInput::default()
        };
        let input = test_input_with_staged(staged);

        let (staged_clusters, main_clusters) = partition_clusters_by_source(
            &input,
            Some(&[XtreamCluster::Live, XtreamCluster::Video]),
            &[],
        );

        assert!(staged_clusters.is_empty());
        assert_eq!(main_clusters, vec![XtreamCluster::Video]);
    }
}
