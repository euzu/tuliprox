use crate::provider::PlaylistFetch;
use chrono::{DateTime, Utc};
use log::{error, info, warn};
use shared::{
    concat_string,
    error::TuliproxError,
    model::{
        EventMessage, EventSink, InputType, PlaylistEntry, PlaylistGroup, PlaylistItem, ProviderAccountEvent,
        ProviderAccountState, ProviderId, ProxyUserStatus, SeriesStreamProperties, StreamProperties,
        VideoStreamProperties, XtreamCluster, XtreamLoginInfo, XtreamPlaylistItem, XtreamSeriesInfo, XtreamVideoInfo,
        XtreamVideoInfoDoc,
    },
    utils::{
        extract_extension_from_url, get_i64_from_serde_value, get_string_from_serde_value, sanitize_sensitive_info,
        Internable, PROVIDER_SCHEME_PREFIX,
    },
};
use std::{collections::HashMap, future::Future, io::Error, str::FromStr, sync::Arc};
use tuliprox_core::{
    model::{
        evaluate_update_quality, is_input_expired, xtream_mapping_option_from_target_options, AppConfig,
        ClusterUpdateRejection, ConfigInput, ConfigInputFlags, ConfigTarget, InputSource, ProxyUserCredentials,
        UpdateQualityDecision, XtreamTargetOutput,
    },
    utils::request,
};
use tuliprox_parser::{xtream, xtream::parse_xtream_series_info};
use tuliprox_repository::{
    count_input_xtream_cluster, get_input_storage_path, get_target_id_mapping, get_target_storage_path,
    persist_input_vod_info, persist_input_xtream_playlist_clusters_to_disk, persists_input_series_info,
    rewrite_provider_series_info_episode_virtual_id, write_playlist_batch_item_upsert, write_playlist_item_update,
    PlaylistStorageState, ProviderEpisodeKey, VirtualIdRecord, XtreamClusterPublishOutcome,
};

const THREE_DAYS_IN_SECS: i64 = 3 * 24 * 60 * 60;

// Moved to `model::playlist_key`; re-exported for this module's callers.
pub use tuliprox_core::model::get_xtream_stream_url_base;

pub fn get_xtream_player_api_action_url(input: &ConfigInput, action: &str) -> Option<String> {
    if let Some(user_info) = input.get_user_info() {
        Some(format!(
            "{}&action={}",
            get_xtream_stream_url_base(&user_info.base_url, &user_info.username, &user_info.password),
            action
        ))
    } else {
        None
    }
}

pub fn get_xtream_player_api_info_url(input: &ConfigInput, cluster: XtreamCluster, stream_id: u32) -> Option<String> {
    let (action, stream_id_field) = cluster.info_action_and_id_field();
    get_xtream_player_api_action_url(input, action)
        .map(|action_url| format!("{action_url}&{stream_id_field}={stream_id}"))
}

pub async fn get_xtream_stream_info_content(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    input: &InputSource,
    trace_log: bool,
) -> Result<String, Error> {
    match request::download_text_content(app_config, client, input, None, None, trace_log).await {
        Ok((content, _response_url)) => Ok(content),
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn get_xtream_stream_info(
    client: &reqwest::Client,
    app_config: &Arc<AppConfig>,
    playlists: &PlaylistStorageState,
    user: &ProxyUserCredentials,
    input: &ConfigInput,
    target: &ConfigTarget,
    pli: &XtreamPlaylistItem,
    info_url: &str,
    cluster: XtreamCluster,
) -> Result<String, TuliproxError> {
    let xtream_output = target
        .get_xtream_output()
        .ok_or_else(|| TuliproxError::ApiXtream("Unexpected error, missing xtream output".to_string()))?;

    let app_config = &app_config;
    let encrypt_secret = app_config.get_encrypt_secret();
    let options = xtream_mapping_option_from_target_options(target, xtream_output, app_config, user, encrypt_secret)?;

    if let Some(content) = pli.get_resolved_info_document(&options) {
        return serde_json::to_string(&content).map_err(|err| TuliproxError::ApiXtream(format!("{err}")));
    }

    let resolved_url = input.resolve_url(info_url)?;
    let input_source = InputSource::from(input).with_url(resolved_url.to_string());
    if let Ok(content) = get_xtream_stream_info_content(app_config, client, &input_source, false).await {
        if content.is_empty() {
            return Err(TuliproxError::ApiXtream(format!(
                "Provider returned no response for stream with id: {}/{}/{}",
                target.name.replace(' ', "_").as_str(),
                cluster,
                pli.get_virtual_id()
            )));
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
                                if let Err(err) = persist_input_vod_info(
                                    app_config,
                                    &storage_path,
                                    cluster,
                                    &input.name,
                                    provider_id,
                                    &video_stream_props,
                                )
                                .await
                                {
                                    error!("Failed to persist video stream for input {}: {err}", input.name);
                                }

                                // update target playlist
                                let mut vod_pli = pli.clone();
                                vod_pli.additional_properties =
                                    Some(StreamProperties::Video(Box::new(video_stream_props)));

                                if let Err(err) = write_playlist_item_update(app_config, &target.name, &vod_pli).await {
                                    error!("Failed to persist video stream: {err}");
                                }

                                if target.use_memory_cache {
                                    playlists.update_playlist_items(target, vec![&vod_pli]).await;
                                }

                                if let Some(value) = xtream_resolve_stream_info(
                                    app_config,
                                    playlists,
                                    user,
                                    target,
                                    xtream_output,
                                    &vod_pli,
                                ) {
                                    return value;
                                }
                            }
                            Err(err) => {
                                error!("Failed to persist video info for provider id {}: {err}", pli.provider_id);
                            }
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
                                if let Err(err) = persists_input_series_info(
                                    app_config,
                                    &storage_path,
                                    cluster,
                                    &input.name,
                                    provider_id,
                                    &series_stream_props,
                                )
                                .await
                                {
                                    error!("Failed to persist series info for input {}: {err}", input.name);
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
                                let config = &app_config.config.load();
                                match get_target_storage_path(config, target.name.as_str()) {
                                    None => {
                                        error!(
                                            "Failed to get target storage path {}. Can't save episodes",
                                            target.name
                                        );
                                    }
                                    Some(target_path) => {
                                        let mut in_memory_updates = Vec::new();
                                        let mut provider_series: HashMap<Arc<str>, Vec<ProviderEpisodeKey>> =
                                            HashMap::new();
                                        {
                                            let (mut target_id_mapping, _file_lock) = get_target_id_mapping(
                                                app_config,
                                                &target_path,
                                                target.use_memory_cache,
                                            )
                                            .await?;

                                            if let Some(_parent_id) = pli.get_provider_id() {
                                                let category_id = pli.get_category_id().unwrap_or(0);
                                                for episode in &mut episodes {
                                                    let episode_provider_id =
                                                        episode.header.get_provider_id().unwrap_or(0);
                                                    episode.header.virtual_id = target_id_mapping
                                                        .get_and_update_virtual_id(
                                                            &episode.header.uuid,
                                                            episode_provider_id,
                                                            episode.header.item_type,
                                                            pli.virtual_id,
                                                        );
                                                    episode.header.category_id = category_id;
                                                    provider_series.entry(pli.get_uuid().intern()).or_default().push(
                                                        ProviderEpisodeKey {
                                                            provider_id: episode_provider_id,
                                                            virtual_id: episode.header.virtual_id.get(),
                                                        },
                                                    );
                                                    if target.use_memory_cache {
                                                        in_memory_updates.push(VirtualIdRecord::new(
                                                            episode_provider_id,
                                                            episode.header.virtual_id,
                                                            episode.header.item_type,
                                                            pli.virtual_id,
                                                            episode.get_uuid(),
                                                        ));
                                                    }
                                                }
                                            }
                                            if let Err(err) = target_id_mapping.persist() {
                                                error!("Failed to persist target id mapping: {err}");
                                            }
                                        }

                                        let xtream_episodes: Vec<XtreamPlaylistItem> =
                                            episodes.iter().map(XtreamPlaylistItem::from).collect();
                                        if let Err(err) = write_playlist_batch_item_upsert(
                                            app_config,
                                            &target.name,
                                            XtreamCluster::Series,
                                            &xtream_episodes,
                                        )
                                        .await
                                        {
                                            error!("Failed to persist playlist batch item update: {err}");
                                        }

                                        if target.use_memory_cache && !in_memory_updates.is_empty() {
                                            playlists.insert_playlist_items(target, episodes).await;
                                            playlists.update_target_id_mapping(target, in_memory_updates).await;
                                        }

                                        if !provider_series.is_empty() {
                                            let mut series_pli = pli.clone();
                                            series_pli.additional_properties =
                                                Some(StreamProperties::Series(Box::new(series_stream_props)));
                                            rewrite_provider_series_info_episode_virtual_id(
                                                &mut series_pli,
                                                &provider_series,
                                            );
                                            if let Err(err) =
                                                write_playlist_item_update(app_config, &target.name, &series_pli).await
                                            {
                                                error!("Failed to persist series stream: {err}");
                                            }
                                            playlists.update_playlist_items(target, vec![&series_pli]).await;

                                            if let Some(value) = xtream_resolve_stream_info(
                                                app_config,
                                                playlists,
                                                user,
                                                target,
                                                xtream_output,
                                                &series_pli,
                                            ) {
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

    Err(TuliproxError::ApiXtream(format!(
        "Can't find stream with id: {}/{}/{}",
        target.name.replace(' ', "_").as_str(),
        cluster,
        pli.get_virtual_id()
    )))
}

fn xtream_resolve_stream_info(
    app_config: &Arc<AppConfig>,
    _playlists: &PlaylistStorageState,
    user: &ProxyUserCredentials,
    target: &ConfigTarget,
    xtream_output: &XtreamTargetOutput,
    pli: &XtreamPlaylistItem,
) -> Option<Result<String, TuliproxError>> {
    let app_config = &app_config;
    let encrypt_secret = app_config.get_encrypt_secret();
    let options =
        match xtream_mapping_option_from_target_options(target, xtream_output, app_config, user, encrypt_secret) {
            Ok(options) => options,
            Err(err) => return Some(Err(err)),
        };
    if let Some(content) = pli.get_resolved_info_document(&options) {
        return Some(
            serde_json::to_string(&content)
                .map_err(|err| TuliproxError::ApiXtream(format!("Failed to serialize stream info: {err}"))),
        );
    }
    None
}

pub fn get_skip_cluster(input: &ConfigInput) -> Vec<XtreamCluster> {
    let mut skip_cluster = vec![];
    if input.has_flag(ConfigInputFlags::SkipLive) {
        skip_cluster.push(XtreamCluster::Live);
    }
    if input.has_flag(ConfigInputFlags::SkipVod) {
        skip_cluster.push(XtreamCluster::Video);
    }
    if input.has_flag(ConfigInputFlags::SkipSeries) {
        skip_cluster.push(XtreamCluster::Series);
    }
    if skip_cluster.len() == 3 {
        info!("You have skipped all sections from xtream input {}", input.name);
    }
    skip_cluster
}

const ACTIONS: [(XtreamCluster, &str, &str); 3] = [
    (
        XtreamCluster::Live,
        tuliprox_core::model::XC_ACTION_GET_LIVE_CATEGORIES,
        tuliprox_core::model::XC_ACTION_GET_LIVE_STREAMS,
    ),
    (
        XtreamCluster::Video,
        tuliprox_core::model::XC_ACTION_GET_VOD_CATEGORIES,
        tuliprox_core::model::XC_ACTION_GET_VOD_STREAMS,
    ),
    (
        XtreamCluster::Series,
        tuliprox_core::model::XC_ACTION_GET_SERIES_CATEGORIES,
        tuliprox_core::model::XC_ACTION_GET_SERIES,
    ),
];

pub async fn xtream_login<E: EventSink>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    events: &E,
    input: &InputSource,
    username: &str,
) -> Result<Option<XtreamLoginInfo>, TuliproxError> {
    let content = if let Ok(content) = request::get_input_json_content(app_config, client, input, None, false).await {
        content
    } else {
        let input_source_account_info =
            input.with_url(format!("{}&action={}", input.url, tuliprox_core::model::XC_ACTION_GET_ACCOUNT_INFO));
        match request::get_input_json_content(app_config, client, &input_source_account_info, None, false).await {
            Ok(content) => content,
            Err(err) => {
                warn!("Failed to login xtream account {username} {err}");
                return Err(err);
            }
        }
    };

    let mut login_info = XtreamLoginInfo { status: None, exp_date: None };

    if let Some(user_info) = content.get("user_info") {
        if let Some(status_value) = user_info.get("status") {
            if let Some(status) = get_string_from_serde_value(status_value) {
                if let Ok(cur_status) = ProxyUserStatus::from_str(&status) {
                    login_info.status = Some(cur_status);
                    if !matches!(cur_status, ProxyUserStatus::Active | ProxyUserStatus::Trial) {
                        warn!("User status for user {username} is {cur_status:?}");
                        let text = format!("User status for user {username} is {cur_status:?}");
                        events.emit(EventMessage::ProviderAccount(ProviderAccountEvent {
                            state: ProviderAccountState::StatusChanged,
                            username: username.to_string(),
                            provider: input.name.to_string(),
                            status: Some(format!("{cur_status:?}")),
                            expires_at: None,
                            message: text,
                        }));
                    }
                }
            }
        }

        if let Some(exp_value) = user_info.get("exp_date") {
            if let Some(expiration_timestamp) = get_i64_from_serde_value(exp_value) {
                login_info.exp_date = Some(expiration_timestamp);
                notify_account_expire(login_info.exp_date, events, username, &input.name);
            }
        }
    }

    if login_info.exp_date.is_none() && login_info.status.is_none() {
        Ok(None)
    } else {
        Ok(Some(login_info))
    }
}

/// Publish the account-expiry state for `username` on `input_name`.
///
/// Emitting is synchronous and non-blocking, so this no longer awaits: the
/// notification is delivered by whoever subscribes to the bus.
pub fn notify_account_expire<E: EventSink>(exp_date: Option<i64>, events: &E, username: &str, input_name: &str) {
    notify_account_expire_at(Utc::now().timestamp(), exp_date, events, username, input_name);
}

/// [`notify_account_expire`] against a caller-supplied instant.
///
/// The three-day warning window and the expired/expiring split are pure functions of
/// `now_secs`; taking it as a parameter is what makes either branch reachable without
/// waiting for the calendar.
pub fn notify_account_expire_at<E: EventSink>(
    now_secs: i64,
    exp_date: Option<i64>,
    events: &E,
    username: &str,
    input_name: &str,
) {
    if let Some(expiration_timestamp) = exp_date {
        if expiration_timestamp > now_secs {
            let time_left = expiration_timestamp - now_secs;

            if time_left < THREE_DAYS_IN_SECS {
                if let Some(datetime) = DateTime::<Utc>::from_timestamp(expiration_timestamp, 0) {
                    let formatted = datetime.format("%Y-%m-%d %H:%M:%S").to_string();
                    warn!("User account for user {username} expires {formatted}");
                    let text = format!("User account for user {username} expires {formatted}");
                    // The suppression key lives on `ProviderAccountEvent`;
                    // re-checked on every refresh, this would otherwise
                    // notify on each playlist update for the three days
                    // before expiry.
                    events.emit(EventMessage::ProviderAccount(ProviderAccountEvent {
                        state: ProviderAccountState::Expiring,
                        username: username.to_string(),
                        provider: input_name.to_string(),
                        status: None,
                        expires_at: Some(expiration_timestamp),
                        message: text,
                    }));
                }
            }
        } else {
            warn!("User account for user {username} is expired");
            let text = format!("User account for user {username} for provider {input_name} is expired");
            events.emit(EventMessage::ProviderAccount(ProviderAccountEvent {
                state: ProviderAccountState::Expired,
                username: username.to_string(),
                provider: input_name.to_string(),
                status: None,
                expires_at: Some(expiration_timestamp),
                message: text,
            }));
        }
    }
}

/// Returns the requested clusters that are not skipped. Staged inputs are resolved into
/// standalone inputs upstream, so this no longer performs per-cluster source partitioning.
pub fn requested_clusters(requested: Option<&[XtreamCluster]>, skip_cluster: &[XtreamCluster]) -> Vec<XtreamCluster> {
    let all_clusters = [XtreamCluster::Live, XtreamCluster::Video, XtreamCluster::Series];
    all_clusters
        .into_iter()
        .filter(|cluster| {
            let is_requested = requested.is_none_or(|c| c.contains(cluster));
            is_requested && !skip_cluster.contains(cluster)
        })
        .collect()
}

fn apply_cluster_update_quality(
    fetch: &mut PlaylistFetch,
    cluster: XtreamCluster,
    current_count: Option<usize>,
    threshold: u8,
    mut candidate_groups: Vec<PlaylistGroup>,
) {
    let candidate_count = candidate_groups.iter().map(|group| group.channels.len()).sum();
    match evaluate_update_quality(current_count, candidate_count, threshold) {
        UpdateQualityDecision::Disabled
        | UpdateQualityDecision::BootstrapAccepted { .. }
        | UpdateQualityDecision::Accepted { .. } => fetch.groups.append(&mut candidate_groups),
        UpdateQualityDecision::Rejected { current, candidate, threshold, quality } => {
            fetch.quality_rejections.push(ClusterUpdateRejection {
                cluster,
                current_count: current,
                candidate_count: candidate,
                threshold,
                quality,
            });
        }
        UpdateQualityDecision::RejectedWithoutBaseline => {
            fetch.quality_rejections.push(ClusterUpdateRejection {
                cluster,
                current_count: current_count.unwrap_or_default(),
                candidate_count,
                threshold: threshold.min(100),
                quality: 0,
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtreamDownloadScope {
    PlaylistUpdate,
    Direct,
}

struct XtreamDownloadContext {
    source_input_type: InputType,
    scope: XtreamDownloadScope,
}

fn disk_cluster_quality_threshold(scope: XtreamDownloadScope, input: &ConfigInput, cluster: XtreamCluster) -> u8 {
    match scope {
        XtreamDownloadScope::PlaylistUpdate => {
            input.options.as_ref().map_or(0, |options| options.update_quality.threshold(cluster))
        }
        XtreamDownloadScope::Direct => 0,
    }
}

fn record_disk_cluster_publish_outcome(fetch: &mut PlaylistFetch, outcome: XtreamClusterPublishOutcome) {
    match outcome {
        XtreamClusterPublishOutcome::Published => {}
        XtreamClusterPublishOutcome::RetainedPrevious(rejection) => fetch.quality_rejections.push(rejection),
    }
}

struct InMemoryXtreamCandidateWinner {
    group_index: usize,
    item: PlaylistItem,
}

fn normalize_in_memory_xtream_candidate(mut candidate_groups: Vec<PlaylistGroup>) -> Vec<PlaylistGroup> {
    let candidate_count = candidate_groups.iter().map(|group| group.channels.len()).sum();
    let mut winning_position_by_provider_id = HashMap::<ProviderId, usize>::with_capacity(candidate_count);
    let mut candidates = Vec::<Option<InMemoryXtreamCandidateWinner>>::with_capacity(candidate_count);

    for (group_index, group) in candidate_groups.iter_mut().enumerate() {
        for item in std::mem::take(&mut group.channels) {
            let provider_id = ProviderId::new(XtreamPlaylistItem::from(&item).provider_id);
            let candidate_position = candidates.len();
            if let Some(previous_position) = winning_position_by_provider_id.insert(provider_id, candidate_position) {
                candidates[previous_position] = None;
            }
            candidates.push(Some(InMemoryXtreamCandidateWinner { group_index, item }));
        }
    }

    for winner in candidates.into_iter().flatten() {
        candidate_groups[winner.group_index].channels.push(winner.item);
    }
    candidate_groups.retain(|group| !group.channels.is_empty());
    candidate_groups
}

async fn apply_in_memory_cluster_download<B, Fut>(
    scope: XtreamDownloadScope,
    input: &ConfigInput,
    cluster: XtreamCluster,
    mut candidate_groups: Vec<PlaylistGroup>,
    fetch: &mut PlaylistFetch,
    load_baseline_count: B,
) -> Result<(), TuliproxError>
where
    B: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<usize>, TuliproxError>>,
{
    match scope {
        XtreamDownloadScope::Direct => {
            fetch.groups.append(&mut candidate_groups);
            return Ok(());
        }
        XtreamDownloadScope::PlaylistUpdate => {}
    }

    let threshold = input.options.as_ref().map_or(0, |options| options.update_quality.threshold(cluster));
    let current_count = if threshold == 0 {
        None
    } else {
        candidate_groups = normalize_in_memory_xtream_candidate(candidate_groups);
        load_baseline_count().await?
    };
    apply_cluster_update_quality(fetch, cluster, current_count, threshold, candidate_groups);
    Ok(())
}

/// Downloads xtream clusters from a single source (either main input or staged input).
async fn download_xtream_from_source<E: EventSink>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    events: &E,
    input: &ConfigInput,
    input_source: &InputSource,
    clusters: &[XtreamCluster],
    context: XtreamDownloadContext,
) -> PlaylistFetch {
    let (username, password) =
        (input_source.username.as_deref().unwrap_or(""), input_source.password.as_deref().unwrap_or(""));

    let is_provider_url = input_source.url.starts_with(PROVIDER_SCHEME_PREFIX);
    let base_input_url = if is_provider_url {
        input_source.url.clone()
    } else {
        match input.resolve_url(&input_source.url) {
            Ok(url) => url.into_owned(),
            Err(err) => return PlaylistFetch::failed(err),
        }
    };

    let base_url = get_xtream_stream_url_base(&base_input_url, username, password);
    let input_source_login = input_source.with_url(base_url.clone());

    if let Err(err) = xtream_login(app_config, client, events, &input_source_login, username).await {
        error!("Could not log in with xtream user {username} for provider {}. {err}", input.name);
        return PlaylistFetch::failed(err);
    }

    let cfg = app_config.config.load();
    let storage_dir = &cfg.storage_dir;
    let use_disk_based_processing = cfg.disk_based_processing && context.source_input_type.is_xtream();
    let mut fetch = PlaylistFetch::groups(Vec::with_capacity(128)).persisted(use_disk_based_processing);

    let mut disk_cluster_readers = Vec::new();
    for (xtream_cluster, category, stream) in &ACTIONS {
        if !clusters.contains(xtream_cluster) {
            continue;
        }
        let input_source_category = input_source.with_url(concat_string!(&base_url, "&action=", category));
        let input_source_stream = input_source.with_url(concat_string!(&base_url, "&action=", stream));
        let category_file_path = tuliprox_core::utils::prepare_file_path(
            input.persist.as_deref(),
            storage_dir,
            concat_string!(category, "_").as_str(),
        );
        let stream_file_path = tuliprox_core::utils::prepare_file_path(
            input.persist.as_deref(),
            storage_dir,
            concat_string!(stream, "_").as_str(),
        );

        match futures::join!(
            request::get_input_json_content_as_stream(app_config, client, &input_source_category, category_file_path),
            request::get_input_json_content_as_stream(app_config, client, &input_source_stream, stream_file_path)
        ) {
            (Ok(category_content), Ok(stream_content)) => {
                if use_disk_based_processing {
                    let quality_threshold = disk_cluster_quality_threshold(context.scope, input, *xtream_cluster);
                    disk_cluster_readers.push((*xtream_cluster, quality_threshold, category_content, stream_content));
                } else {
                    match xtream::parse_xtream(input, *xtream_cluster, category_content, stream_content).await {
                        Ok(sub_playlist_parsed) => {
                            if let Some(xtream_sub_playlist) = sub_playlist_parsed {
                                if let Err(err) = apply_in_memory_cluster_download(
                                    context.scope,
                                    input,
                                    *xtream_cluster,
                                    xtream_sub_playlist,
                                    &mut fetch,
                                    || count_input_xtream_cluster(app_config, input, *xtream_cluster),
                                )
                                .await
                                {
                                    fetch.errors.push(err);
                                }
                            } else {
                                error!(
                                    "Could not parse playlist {xtream_cluster} for input {}: {}",
                                    input_source.name,
                                    sanitize_sensitive_info(&input_source.url)
                                );
                            }
                        }
                        Err(err) => fetch.errors.push(err),
                    }
                }
            }
            (Err(err1), Err(err2)) => {
                fetch.errors.extend([err1, err2]);
            }
            (_, Err(err)) | (Err(err), _) => fetch.errors.push(err),
        }
    }

    if use_disk_based_processing && fetch.errors.is_empty() && !disk_cluster_readers.is_empty() {
        match persist_input_xtream_playlist_clusters_to_disk(app_config, input, disk_cluster_readers).await {
            Ok(outcomes) => {
                for outcome in outcomes {
                    record_disk_cluster_publish_outcome(&mut fetch, outcome);
                }
            }
            Err(err) => {
                error!("persist_input_xtream_playlist_clusters_to_disk failed: {err}");
                fetch.errors.push(err);
            }
        }
    }

    fetch
}

async fn download_xtream_playlist_with_scope<E: EventSink>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    events: &E,
    input: &ConfigInput,
    clusters: Option<&[XtreamCluster]>,
    scope: XtreamDownloadScope,
) -> PlaylistFetch {
    let skip_cluster = get_skip_cluster(input);
    let main_clusters = requested_clusters(clusters, &skip_cluster);

    let mut fetch = PlaylistFetch::groups(Vec::with_capacity(128));

    if !main_clusters.is_empty() {
        check_alias_user_state(events, input);
        let source: InputSource = input.into();
        let mut source_fetch = download_xtream_from_source(
            app_config,
            client,
            events,
            input,
            &source,
            &main_clusters,
            XtreamDownloadContext { source_input_type: input.input_type, scope },
        )
        .await;
        fetch.groups.append(&mut source_fetch.groups);
        fetch.errors.append(&mut source_fetch.errors);
        fetch.quality_rejections.append(&mut source_fetch.quality_rejections);
        fetch.persisted |= source_fetch.persisted;
    }

    for (grp_id, plg) in (1_u32..).zip(fetch.groups.iter_mut()) {
        plg.id = grp_id;
    }

    fetch
}

/// Downloads an Xtream candidate for the playlist-update flow and applies the configured quality policy.
pub async fn download_xtream_playlist<E: EventSink>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    events: &E,
    input: &ConfigInput,
    clusters: Option<&[XtreamCluster]>,
) -> PlaylistFetch {
    download_xtream_playlist_with_scope(
        app_config,
        client,
        events,
        input,
        clusters,
        XtreamDownloadScope::PlaylistUpdate,
    )
    .await
}

/// Downloads Xtream data for a direct caller without consulting the persisted update baseline.
pub async fn download_xtream_playlist_direct<E: EventSink>(
    app_config: &Arc<AppConfig>,
    client: &reqwest::Client,
    events: &E,
    input: &ConfigInput,
    clusters: Option<&[XtreamCluster]>,
) -> PlaylistFetch {
    download_xtream_playlist_with_scope(app_config, client, events, input, clusters, XtreamDownloadScope::Direct).await
}

fn check_alias_user_state<E: EventSink>(events: &E, input: &ConfigInput) {
    if let Some(aliases) = input.aliases.as_ref() {
        for alias in aliases {
            if is_input_expired(alias.exp_date) {
                notify_account_expire(
                    alias.exp_date,
                    events,
                    alias.username.as_ref().map_or("", |s| s.as_str()),
                    &alias.name,
                );
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

pub fn create_vod_info_from_item(pli: &XtreamPlaylistItem) -> String {
    let category_id = pli.category_id;
    let added = pli.additional_properties.as_ref().and_then(StreamProperties::get_last_modified).unwrap_or(0);
    let name = &pli.name;
    let extension: String = pli
        .get_container_extension()
        .filter(|ce| !ce.is_empty())
        .map(|s| s.to_string())
        // `extract_extension_from_url` keeps the leading dot; `container_extension` must not.
        .or_else(|| extract_extension_from_url(&pli.url).map(|ext| ext.trim_start_matches('.').to_string()))
        .unwrap_or_default();

    let mut doc = XtreamVideoInfoDoc::default();
    doc.info.name.clone_from(name);
    doc.movie_data.stream_id = pli.virtual_id.get();
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
    use super::{
        apply_cluster_update_quality, apply_in_memory_cluster_download, create_vod_info_from_item,
        disk_cluster_quality_threshold, record_disk_cluster_publish_outcome, requested_clusters, XtreamDownloadScope,
    };
    use crate::provider::PlaylistFetch;
    use serde_json::Value;
    use shared::{
        model::{
            ConfigInputOptionsDto, ConfigInputUpdateQualityDto, InputType, PlaylistGroup, PlaylistItem,
            PlaylistItemHeader, PlaylistItemType, ProxyType, XtreamCluster, XtreamPlaylistItem,
        },
        utils::Internable,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tuliprox_core::model::{
        ClusterUpdateRejection, ConfigInput, ConfigInputFlags, ConfigInputFlagsSet, ConfigInputOptions,
        ProxyUserCredentials,
    };
    use tuliprox_repository::XtreamClusterPublishOutcome;

    /// Records what reached the bus, so the expiry branches can be asserted rather than
    /// inferred from a log line.
    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<shared::model::ProviderAccountEvent>>);

    impl shared::model::EventSink for RecordingSink {
        fn emit(&self, event: shared::model::EventMessage) {
            if let shared::model::EventMessage::ProviderAccount(event) = event {
                self.0.lock().expect("sink poisoned").push(event);
            }
        }
    }

    impl RecordingSink {
        fn states(&self) -> Vec<shared::model::ProviderAccountState> {
            self.0.lock().expect("sink poisoned").iter().map(|event| event.state).collect()
        }
    }

    const EXPIRY: i64 = 1_700_000_000;

    #[test]
    fn an_account_past_its_expiry_is_reported_expired() {
        let sink = RecordingSink::default();
        super::notify_account_expire_at(EXPIRY + 1, Some(EXPIRY), &sink, "user", "provider");
        assert_eq!(sink.states(), vec![shared::model::ProviderAccountState::Expired]);
    }

    #[test]
    fn an_account_inside_the_three_day_window_is_reported_expiring() {
        let sink = RecordingSink::default();
        super::notify_account_expire_at(EXPIRY - 60, Some(EXPIRY), &sink, "user", "provider");
        assert_eq!(sink.states(), vec![shared::model::ProviderAccountState::Expiring]);
    }

    #[test]
    fn an_account_outside_the_three_day_window_is_quiet() {
        let sink = RecordingSink::default();
        // One second before the window opens.
        super::notify_account_expire_at(EXPIRY - super::THREE_DAYS_IN_SECS - 1, Some(EXPIRY), &sink, "u", "p");
        assert!(sink.states().is_empty());
    }

    #[test]
    fn no_expiry_date_emits_nothing() {
        let sink = RecordingSink::default();
        super::notify_account_expire_at(EXPIRY, None, &sink, "user", "provider");
        assert!(sink.states().is_empty());
    }

    fn options_with_flags(flags: &[ConfigInputFlags]) -> ConfigInputOptions {
        let mut set = ConfigInputFlagsSet::new();
        for flag in flags {
            set.set(*flag);
        }
        ConfigInputOptions { flags: set, ..ConfigInputOptions::defaults().clone() }
    }

    #[test]
    fn requested_clusters_respects_skip_flags() {
        let input = ConfigInput {
            name: "test".intern(),
            input_type: InputType::Xtream,
            options: Some(options_with_flags(&[ConfigInputFlags::SkipLive])),
            ..ConfigInput::default()
        };

        let skip_cluster = super::get_skip_cluster(&input);
        let clusters = requested_clusters(None, &skip_cluster);

        assert!(!clusters.contains(&XtreamCluster::Live));
        assert!(clusters.contains(&XtreamCluster::Video));
        assert!(clusters.contains(&XtreamCluster::Series));
    }

    #[test]
    fn requested_clusters_filters_by_requested_set() {
        let clusters = requested_clusters(Some(&[XtreamCluster::Live, XtreamCluster::Video]), &[]);
        assert_eq!(clusters, vec![XtreamCluster::Live, XtreamCluster::Video]);
    }

    #[test]
    fn requested_clusters_excludes_skip_clusters() {
        let clusters = requested_clusters(None, &[XtreamCluster::Series]);
        assert_eq!(clusters, vec![XtreamCluster::Live, XtreamCluster::Video]);
    }

    fn candidate_group(cluster: XtreamCluster, category_id: u32, count: usize) -> PlaylistGroup {
        let title = format!("{cluster}-{category_id}").intern();
        let channels = (0..count)
            .map(|index| {
                let stream_id = format!("{category_id}{index:03}").intern();
                PlaylistItem {
                    header: PlaylistItemHeader {
                        id: Arc::clone(&stream_id),
                        input_stream_id: stream_id,
                        name: format!("stream-{index}").intern(),
                        title: format!("stream-{index}").intern(),
                        group: Arc::clone(&title),
                        url: format!("http://provider.example/{cluster}/{index}").intern(),
                        item_type: PlaylistItemType::from(cluster),
                        xtream_cluster: cluster,
                        category_id,
                        input_name: "provider".intern(),
                        ..PlaylistItemHeader::default()
                    },
                }
            })
            .collect();
        PlaylistGroup { id: category_id, title, channels, xtream_cluster: cluster }
    }

    fn set_candidate_provider_id(item: &mut PlaylistItem, provider_id: u32) {
        let provider_id = provider_id.to_string().intern();
        item.header.id = Arc::clone(&provider_id);
        item.header.input_stream_id = provider_id;
    }

    #[tokio::test]
    async fn in_memory_quality_direct_download_skips_baseline_while_update_rejects_same_candidate() {
        let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, ..ConfigInputUpdateQualityDto::default() },
            ..ConfigInputOptionsDto::default()
        });
        let input = ConfigInput {
            name: "provider".intern(),
            input_type: InputType::Xtream,
            options: Some(options),
            ..ConfigInput::default()
        };
        let candidate = vec![candidate_group(XtreamCluster::Live, 1, 89)];
        let baseline_reads = AtomicUsize::new(0);

        let mut direct_fetch = PlaylistFetch::default();
        apply_in_memory_cluster_download(
            XtreamDownloadScope::Direct,
            &input,
            XtreamCluster::Live,
            candidate.clone(),
            &mut direct_fetch,
            || async {
                baseline_reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(100))
            },
        )
        .await
        .expect("direct candidate should be accepted");

        assert_eq!(baseline_reads.load(Ordering::SeqCst), 0);
        assert_eq!(direct_fetch.groups.len(), 1);
        assert_eq!(direct_fetch.groups[0].channels.len(), 89);
        assert!(direct_fetch.quality_rejections.is_empty());

        let mut update_fetch = PlaylistFetch::default();
        apply_in_memory_cluster_download(
            XtreamDownloadScope::PlaylistUpdate,
            &input,
            XtreamCluster::Live,
            candidate,
            &mut update_fetch,
            || async {
                baseline_reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(100))
            },
        )
        .await
        .expect("update candidate should produce a domain decision");

        assert_eq!(baseline_reads.load(Ordering::SeqCst), 1);
        assert!(update_fetch.groups.is_empty());
        assert_eq!(
            update_fetch.quality_rejections,
            vec![tuliprox_core::model::ClusterUpdateRejection {
                cluster: XtreamCluster::Live,
                current_count: 100,
                candidate_count: 89,
                threshold: 90,
                quality: 89,
            }]
        );
    }

    #[tokio::test]
    async fn in_memory_quality_deduplicates_provider_ids_and_keeps_the_last_group() {
        let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 100, ..ConfigInputUpdateQualityDto::default() },
            ..ConfigInputOptionsDto::default()
        });
        let input = ConfigInput {
            name: "provider".intern(),
            input_type: InputType::Xtream,
            options: Some(options),
            ..ConfigInput::default()
        };
        let mut first_group = candidate_group(XtreamCluster::Live, 1, 1);
        set_candidate_provider_id(&mut first_group.channels[0], 7);
        let mut winning_group = candidate_group(XtreamCluster::Live, 2, 2);
        set_candidate_provider_id(&mut winning_group.channels[0], 7);
        set_candidate_provider_id(&mut winning_group.channels[1], 8);
        winning_group.channels[0].header.name = "winning-duplicate".intern();
        winning_group.channels[0].header.title = "winning-duplicate".intern();
        let baseline_reads = AtomicUsize::new(0);
        let mut fetch = PlaylistFetch::default();

        apply_in_memory_cluster_download(
            XtreamDownloadScope::PlaylistUpdate,
            &input,
            XtreamCluster::Live,
            vec![first_group, winning_group],
            &mut fetch,
            || async {
                baseline_reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(2))
            },
        )
        .await
        .expect("deduplicated candidate should be accepted");

        assert_eq!(baseline_reads.load(Ordering::SeqCst), 1);
        assert!(fetch.quality_rejections.is_empty());
        assert_eq!(fetch.groups.len(), 1, "the emptied first category must be removed");
        assert_eq!(fetch.groups[0].id, 2);
        assert_eq!(fetch.groups[0].channels.len(), 2);
        let winner =
            fetch.groups[0].channels.iter().find(|item| item.header.id.as_ref() == "7").expect("winning duplicate");
        assert_eq!(winner.header.name.as_ref(), "winning-duplicate");
        assert_eq!(winner.header.category_id, 2);
    }

    #[tokio::test]
    async fn in_memory_quality_direct_and_disabled_paths_preserve_duplicate_rows_without_baseline_io() {
        let guarded_options = ConfigInputOptions::from(&ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 100, ..ConfigInputUpdateQualityDto::default() },
            ..ConfigInputOptionsDto::default()
        });
        let guarded_input = ConfigInput {
            name: "provider".intern(),
            input_type: InputType::Xtream,
            options: Some(guarded_options),
            ..ConfigInput::default()
        };
        let disabled_input = ConfigInput {
            name: "provider".intern(),
            input_type: InputType::Xtream,
            options: Some(ConfigInputOptions::from(&ConfigInputOptionsDto::default())),
            ..ConfigInput::default()
        };
        let mut first_group = candidate_group(XtreamCluster::Live, 1, 1);
        set_candidate_provider_id(&mut first_group.channels[0], 7);
        let mut second_group = candidate_group(XtreamCluster::Live, 2, 1);
        set_candidate_provider_id(&mut second_group.channels[0], 7);
        let candidate = vec![first_group, second_group];
        let baseline_reads = AtomicUsize::new(0);

        let mut direct_fetch = PlaylistFetch::default();
        apply_in_memory_cluster_download(
            XtreamDownloadScope::Direct,
            &guarded_input,
            XtreamCluster::Live,
            candidate.clone(),
            &mut direct_fetch,
            || async {
                baseline_reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(2))
            },
        )
        .await
        .expect("direct candidate should remain unguarded");

        let mut disabled_fetch = PlaylistFetch::default();
        apply_in_memory_cluster_download(
            XtreamDownloadScope::PlaylistUpdate,
            &disabled_input,
            XtreamCluster::Live,
            candidate,
            &mut disabled_fetch,
            || async {
                baseline_reads.fetch_add(1, Ordering::SeqCst);
                Ok(Some(2))
            },
        )
        .await
        .expect("disabled quality guard should preserve the candidate");

        assert_eq!(baseline_reads.load(Ordering::SeqCst), 0);
        assert_eq!(direct_fetch.groups.iter().map(|group| group.channels.len()).sum::<usize>(), 2);
        assert_eq!(disabled_fetch.groups.iter().map(|group| group.channels.len()).sum::<usize>(), 2);
        assert!(direct_fetch.quality_rejections.is_empty());
        assert!(disabled_fetch.quality_rejections.is_empty());
    }

    #[test]
    fn in_memory_quality_applies_exact_90_percent_boundaries() {
        for (name, candidate_count, rejected) in [
            ("lower boundary", 90, false),
            ("below lower boundary", 89, true),
            ("upper boundary", 110, false),
            ("above upper boundary", 111, true),
        ] {
            let mut fetch = PlaylistFetch::default();
            apply_cluster_update_quality(
                &mut fetch,
                XtreamCluster::Live,
                Some(100),
                90,
                vec![candidate_group(XtreamCluster::Live, 1, candidate_count)],
            );

            assert_eq!(!fetch.quality_rejections.is_empty(), rejected, "case: {name}");
            assert_eq!(fetch.groups.is_empty(), rejected, "case: {name}");
            if rejected {
                let rejection = fetch.quality_rejections.first().expect("rejection report");
                assert_eq!(rejection.current_count, 100, "case: {name}");
                assert_eq!(rejection.candidate_count, candidate_count, "case: {name}");
                assert_eq!(rejection.threshold, 90, "case: {name}");
                assert_eq!(rejection.quality, 89, "case: {name}");
            }
        }
    }

    #[test]
    fn in_memory_quality_preserves_disabled_and_bootstrap_behavior() {
        let mut disabled = PlaylistFetch::default();
        apply_cluster_update_quality(
            &mut disabled,
            XtreamCluster::Live,
            Some(100),
            0,
            vec![candidate_group(XtreamCluster::Live, 1, 1)],
        );
        assert_eq!(disabled.groups[0].channels.len(), 1);
        assert!(disabled.quality_rejections.is_empty());

        let mut bootstrap = PlaylistFetch::default();
        apply_cluster_update_quality(
            &mut bootstrap,
            XtreamCluster::Video,
            None,
            90,
            vec![candidate_group(XtreamCluster::Video, 2, 3)],
        );
        assert_eq!(bootstrap.groups[0].channels.len(), 3);
        assert!(bootstrap.quality_rejections.is_empty());

        let mut empty_bootstrap = PlaylistFetch::default();
        apply_cluster_update_quality(&mut empty_bootstrap, XtreamCluster::Series, None, 90, Vec::new());
        assert!(empty_bootstrap.groups.is_empty());
        assert_eq!(
            empty_bootstrap.quality_rejections,
            vec![tuliprox_core::model::ClusterUpdateRejection {
                cluster: XtreamCluster::Series,
                current_count: 0,
                candidate_count: 0,
                threshold: 90,
                quality: 0,
            }]
        );
    }

    #[test]
    fn rejected_vod_does_not_block_accepted_live_and_series_clusters() {
        let mut fetch = PlaylistFetch::default();
        apply_cluster_update_quality(
            &mut fetch,
            XtreamCluster::Live,
            Some(100),
            90,
            vec![candidate_group(XtreamCluster::Live, 1, 90)],
        );
        apply_cluster_update_quality(
            &mut fetch,
            XtreamCluster::Video,
            Some(100),
            90,
            vec![candidate_group(XtreamCluster::Video, 2, 40), candidate_group(XtreamCluster::Video, 3, 49)],
        );
        apply_cluster_update_quality(
            &mut fetch,
            XtreamCluster::Series,
            Some(100),
            90,
            vec![candidate_group(XtreamCluster::Series, 4, 110)],
        );

        assert_eq!(fetch.groups.len(), 2);
        assert!(fetch.groups.iter().any(|group| group.xtream_cluster == XtreamCluster::Live));
        assert!(fetch.groups.iter().any(|group| group.xtream_cluster == XtreamCluster::Series));
        assert!(!fetch.groups.iter().any(|group| group.xtream_cluster == XtreamCluster::Video));
        assert_eq!(fetch.quality_rejections.len(), 1);
        assert_eq!(fetch.quality_rejections[0].cluster, XtreamCluster::Video);
        assert_eq!(fetch.quality_rejections[0].candidate_count, 89);
    }

    #[test]
    fn disk_publish_outcomes_transport_rejection_without_blocking_published_clusters() {
        let rejection = ClusterUpdateRejection {
            cluster: XtreamCluster::Video,
            current_count: 100,
            candidate_count: 89,
            threshold: 90,
            quality: 89,
        };
        let mut fetch = PlaylistFetch::default().persisted(true);

        for outcome in [
            XtreamClusterPublishOutcome::Published,
            XtreamClusterPublishOutcome::RetainedPrevious(rejection),
            XtreamClusterPublishOutcome::Published,
        ] {
            record_disk_cluster_publish_outcome(&mut fetch, outcome);
        }

        assert!(fetch.persisted);
        assert!(fetch.errors.is_empty());
        assert!(fetch.groups.is_empty());
        assert_eq!(fetch.quality_rejections, vec![rejection]);
    }

    #[test]
    fn disk_quality_scope_disables_guard_for_direct_downloads() {
        let options = ConfigInputOptions::from(&ConfigInputOptionsDto {
            update_quality: ConfigInputUpdateQualityDto { live: 90, ..ConfigInputUpdateQualityDto::default() },
            ..ConfigInputOptionsDto::default()
        });
        let input = ConfigInput { options: Some(options), ..ConfigInput::default() };

        assert_eq!(
            disk_cluster_quality_threshold(XtreamDownloadScope::PlaylistUpdate, &input, XtreamCluster::Live),
            90
        );
        assert_eq!(disk_cluster_quality_threshold(XtreamDownloadScope::Direct, &input, XtreamCluster::Live), 0);
    }

    fn test_vod_item() -> XtreamPlaylistItem {
        XtreamPlaylistItem {
            virtual_id: shared::model::VirtualId::new(176_141),
            provider_id: 813_563,
            name: "Movie".intern(),
            logo: "".intern(),
            logo_small: "".intern(),
            group: "".intern(),
            title: "Movie".intern(),
            parent_code: "".intern(),
            rec: "".intern(),
            url: "http://provider.example/movie/u/p/813563.mp4".intern(),
            epg_channel_id: None,
            xtream_cluster: XtreamCluster::Video,
            additional_properties: None,
            item_type: PlaylistItemType::Video,
            category_id: 7,
            input_name: "provider".intern(),
            channel_no: 1,
            source_ordinal: 1,
            input_stream_id: "813563".intern(),
            upstream_user_agent: None,
        }
    }

    fn assert_vod_info_keeps_tuliprox_virtual_stream_id(proxy: ProxyType) {
        let mut user = ProxyUserCredentials::default();
        user.proxy = proxy;
        assert_eq!(user.proxy, proxy);
        let pli = test_vod_item();

        let content = create_vod_info_from_item(&pli);
        let doc: Value = serde_json::from_str(&content).expect("valid VOD info JSON");

        assert_eq!(doc["movie_data"]["stream_id"], 176_141);
    }

    #[test]
    fn create_vod_info_keeps_tuliprox_virtual_stream_id_for_redirect_users() {
        assert_vod_info_keeps_tuliprox_virtual_stream_id(ProxyType::Redirect);
    }

    #[test]
    fn create_vod_info_keeps_tuliprox_virtual_stream_id_for_reverse_users() {
        assert_vod_info_keeps_tuliprox_virtual_stream_id(ProxyType::Reverse(None));
    }
}
